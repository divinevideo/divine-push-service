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

use redis::aio::MultiplexedConnection;
use redis::{Client, ErrorKind, IntoConnectionInfo, RedisError, Value};
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
    type Connection = MultiplexedConnection;
    type Error = RedisError;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        ensure_primary(&mut conn).await?;
        Ok(conn)
    }

    async fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        ensure_primary(conn).await
    }

    fn has_broken(&self, _: &mut Self::Connection) -> bool {
        false
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
    fn malformed_role_reply_is_rejected() {
        assert!(check_role(&Value::Nil).is_err());
        assert!(check_role(&Value::Array(vec![])).is_err());
    }
}
