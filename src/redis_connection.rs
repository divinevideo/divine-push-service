//! bb8 connection manager that only hands out connections to a Redis primary.
//!
//! The stock `bb8_redis::RedisConnectionManager` validates pooled connections
//! with `PING`, which a replica answers normally. After a primary change, every
//! pooled connection still points at the demoted node and each write fails with
//! `READONLY` until bb8 recycles the connection on its idle or lifetime timer.
//! This manager checks `ROLE` instead, both on connect and on checkout. A pooled
//! connection to a node that is no longer the primary fails checkout validation
//! and is dropped, and the replacement connects through the URL to the current
//! primary. Rejecting a replica in `connect` as well puts reconnects behind
//! bb8's retry backoff while the URL still resolves to the demoted node,
//! instead of a tight connect-and-reject loop.
//!
//! The role check cannot run when the server refuses `ROLE`, and it only runs
//! between checkouts. Pooled connections therefore also watch their replies:
//! one that sees a `READONLY` error reports itself broken, and bb8 drops it
//! when it returns to the pool instead of handing it out again.

use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::{Client, Cmd, ErrorKind, IntoConnectionInfo, Pipeline, RedisError, RedisFuture, Value};
use tracing::warn;

/// A `bb8::ManageConnection` that rejects connections to a Redis replica.
#[derive(Clone, Debug)]
pub struct PrimaryConnectionManager {
    client: Client,
}

impl PrimaryConnectionManager {
    /// Create a manager for the given Redis URL or connection info.
    pub fn new<T: IntoConnectionInfo>(info: T) -> Result<Self, RedisError> {
        Ok(Self {
            client: Client::open(info.into_connection_info()?)?,
        })
    }
}

impl bb8_redis::bb8::ManageConnection for PrimaryConnectionManager {
    type Connection = PrimaryConnection;
    type Error = RedisError;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        ensure_primary(&mut conn).await?;
        Ok(PrimaryConnection {
            inner: conn,
            saw_read_only: false,
        })
    }

    async fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        ensure_primary(&mut conn.inner).await
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        if conn.saw_read_only {
            warn!("Dropping pooled Redis connection that received a READONLY reply");
        }
        conn.saw_read_only
    }
}

/// A pooled Redis connection that remembers whether a reply was `READONLY`.
pub struct PrimaryConnection {
    inner: MultiplexedConnection,
    saw_read_only: bool,
}

impl ConnectionLike for PrimaryConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        Box::pin(async move {
            let result = self.inner.req_packed_command(cmd).await;
            self.saw_read_only |= match &result {
                Ok(value) => value_is_read_only(value),
                Err(e) => error_is_read_only(e),
            };
            result
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let result = self.inner.req_packed_commands(cmd, offset, count).await;
            self.saw_read_only |= match &result {
                Ok(values) => values.iter().any(value_is_read_only),
                Err(e) => error_is_read_only(e),
            };
            result
        })
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

fn error_is_read_only(e: &RedisError) -> bool {
    e.kind() == ErrorKind::ReadOnly || e.code() == Some("READONLY")
}

/// Finds a `READONLY` error anywhere in a reply, including inside a pipeline
/// or `EXEC` result, where errors arrive as values rather than as `Err`.
fn value_is_read_only(value: &Value) -> bool {
    match value {
        Value::ServerError(e) => e.code() == "READONLY",
        Value::Array(items) => items.iter().any(value_is_read_only),
        _ => false,
    }
}

async fn ensure_primary(conn: &mut MultiplexedConnection) -> Result<(), RedisError> {
    match redis::cmd("ROLE").query_async::<Value>(conn).await {
        Ok(reply) => check_role(&reply),
        // The server answered but refused ROLE (an ACL without it, or a proxy
        // that does not implement it). Keep the previous PING check rather
        // than failing every connection.
        Err(e) if e.code().is_some() => ping(conn).await,
        Err(e) => Err(e),
    }
}

async fn ping(conn: &mut MultiplexedConnection) -> Result<(), RedisError> {
    let pong: String = redis::cmd("PING").query_async(conn).await?;
    match pong.as_str() {
        "PONG" => Ok(()),
        _ => Err((ErrorKind::ResponseError, "ping request").into()),
    }
}

/// Accepts a `ROLE` reply only when the node reports itself as the primary.
fn check_role(reply: &Value) -> Result<(), RedisError> {
    let role = match reply {
        Value::Array(items) => items.first().and_then(|first| match first {
            Value::BulkString(bytes) => std::str::from_utf8(bytes).ok(),
            Value::SimpleString(s) => Some(s.as_str()),
            _ => None,
        }),
        _ => None,
    };

    match role {
        Some("master") => Ok(()),
        Some(other) => {
            warn!(
                role = other,
                "Dropping pooled Redis connection to a node that is not the primary"
            );
            Err(RedisError::from((
                ErrorKind::ReadOnly,
                "pooled connection is not to the Redis primary",
                other.to_string(),
            )))
        }
        None => Err((ErrorKind::TypeError, "unexpected ROLE reply").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role_reply(role: &str) -> Value {
        Value::Array(vec![
            Value::BulkString(role.as_bytes().to_vec()),
            Value::Int(0),
        ])
    }

    #[test]
    fn primary_role_is_valid() {
        assert!(check_role(&role_reply("master")).is_ok());
    }

    #[test]
    fn replica_role_is_rejected_as_read_only() {
        let err = check_role(&role_reply("slave")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ReadOnly);
    }

    #[test]
    fn sentinel_role_is_rejected() {
        assert!(check_role(&role_reply("sentinel")).is_err());
    }

    #[test]
    fn read_only_reply_is_detected_anywhere_in_a_reply() {
        let read_only = || {
            redis::parse_redis_value(b"-READONLY You can't write against a read only replica.\r\n")
                .unwrap()
        };
        assert!(value_is_read_only(&read_only()));
        assert!(value_is_read_only(&Value::Array(vec![
            Value::Okay,
            Value::Array(vec![read_only()]),
        ])));
        assert!(!value_is_read_only(&Value::Okay));
        assert!(error_is_read_only(&RedisError::from((
            ErrorKind::ReadOnly,
            "READONLY"
        ))));
        assert!(!error_is_read_only(&RedisError::from((
            ErrorKind::ResponseError,
            "ERR"
        ))));
    }

    #[test]
    fn malformed_role_reply_is_rejected() {
        assert!(check_role(&Value::Nil).is_err());
        assert!(check_role(&Value::Array(vec![])).is_err());
    }
}
