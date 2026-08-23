//! Persistent store for passkey dashboard authentication.

use openfang_types::error::{OpenFangError, OpenFangResult};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct DashboardUser {
    pub id: String,
    pub slug: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct EnrollmentInvite {
    pub id: String,
    pub user: DashboardUser,
    pub expires_at: i64,
}

#[derive(Debug, Clone)]
pub struct StoredPasskey {
    pub credential_id: Vec<u8>,
    pub user: DashboardUser,
    pub passkey_json: String,
}

#[derive(Debug, Clone)]
pub struct SessionPrincipal {
    pub user: DashboardUser,
    pub credential_id: Vec<u8>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SlotStatus {
    pub slug: String,
    pub display_name: String,
    pub active_credentials: u64,
    pub pending_invite_expires_at: Option<i64>,
}

#[derive(Clone)]
pub struct DashboardAuthStore {
    conn: Arc<Mutex<Connection>>,
}

impl DashboardAuthStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub fn ensure_slot(
        &self,
        slug: &str,
        display_name: &str,
        now: i64,
    ) -> OpenFangResult<DashboardUser> {
        validate_slug(slug)?;
        if display_name.trim().is_empty() {
            return Err(memory_error("display name must not be empty"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO dashboard_auth_users (id, slug, display_name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, slug, display_name.trim(), now],
        )
        .map_err(db_error)?;
        conn.execute(
            "UPDATE dashboard_auth_users SET display_name = ?1 WHERE slug = ?2",
            params![display_name.trim(), slug],
        )
        .map_err(db_error)?;
        load_user_by_slug(&conn, slug)?.ok_or_else(|| memory_error("failed to create passkey slot"))
    }

    pub fn create_invite(
        &self,
        slug: &str,
        display_name: &str,
        token_hash: &[u8],
        now: i64,
        expires_at: i64,
    ) -> OpenFangResult<EnrollmentInvite> {
        if expires_at <= now {
            return Err(memory_error("invite expiry must be in the future"));
        }
        let user = self.ensure_slot(slug, display_name, now)?;
        let invite_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_error)?;
        let active: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM dashboard_auth_passkeys WHERE user_id = ?1 AND revoked_at IS NULL",
                params![user.id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if active > 0 {
            return Err(memory_error(format!(
                "slot '{}' already has an active passkey; reset it first",
                slug
            )));
        }
        tx.execute(
            "UPDATE dashboard_auth_invites SET revoked_at = ?1 WHERE user_id = ?2 AND consumed_at IS NULL AND revoked_at IS NULL",
            params![now, user.id],
        )
        .map_err(db_error)?;
        tx.execute(
            "INSERT INTO dashboard_auth_invites (id, user_id, token_hash, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![invite_id, user.id, token_hash, now, expires_at],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(EnrollmentInvite {
            id: invite_id,
            user,
            expires_at,
        })
    }

    pub fn find_valid_invite(
        &self,
        token_hash: &[u8],
        now: i64,
    ) -> OpenFangResult<Option<EnrollmentInvite>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT i.id, u.id, u.slug, u.display_name, i.expires_at
             FROM dashboard_auth_invites i
             JOIN dashboard_auth_users u ON u.id = i.user_id
             WHERE i.token_hash = ?1 AND i.consumed_at IS NULL AND i.revoked_at IS NULL AND i.expires_at > ?2",
            params![token_hash, now],
            |row| {
                Ok(EnrollmentInvite {
                    id: row.get(0)?,
                    user: DashboardUser { id: row.get(1)?, slug: row.get(2)?, display_name: row.get(3)? },
                    expires_at: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(db_error)
    }

    pub fn consume_invite_and_store_passkey(
        &self,
        invite_id: &str,
        credential_id: &[u8],
        passkey_json: &str,
        now: i64,
    ) -> OpenFangResult<DashboardUser> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_error)?;
        let user_id: Option<String> = tx
            .query_row(
                "SELECT user_id FROM dashboard_auth_invites WHERE id = ?1 AND consumed_at IS NULL AND revoked_at IS NULL AND expires_at > ?2",
                params![invite_id, now],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let user_id =
            user_id.ok_or_else(|| memory_error("invite is expired, revoked, or already used"))?;
        let active: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM dashboard_auth_passkeys WHERE user_id = ?1 AND revoked_at IS NULL",
                params![user_id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if active > 0 {
            return Err(memory_error("slot already has an active passkey"));
        }
        tx.execute(
            "INSERT INTO dashboard_auth_passkeys (credential_id, user_id, passkey_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![credential_id, user_id, passkey_json, now],
        )
        .map_err(db_error)?;
        let changed = tx
            .execute(
                "UPDATE dashboard_auth_invites SET consumed_at = ?1 WHERE id = ?2 AND consumed_at IS NULL AND revoked_at IS NULL",
                params![now, invite_id],
            )
            .map_err(db_error)?;
        if changed != 1 {
            return Err(memory_error("invite was consumed concurrently"));
        }
        let user = load_user_by_id(&tx, &user_id)?
            .ok_or_else(|| memory_error("passkey user is missing"))?;
        tx.commit().map_err(db_error)?;
        Ok(user)
    }

    pub fn active_passkeys(&self) -> OpenFangResult<Vec<StoredPasskey>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT p.credential_id, p.passkey_json, u.id, u.slug, u.display_name
                 FROM dashboard_auth_passkeys p
                 JOIN dashboard_auth_users u ON u.id = p.user_id
                 WHERE p.revoked_at IS NULL ORDER BY u.slug",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredPasskey {
                    credential_id: row.get(0)?,
                    passkey_json: row.get(1)?,
                    user: DashboardUser {
                        id: row.get(2)?,
                        slug: row.get(3)?,
                        display_name: row.get(4)?,
                    },
                })
            })
            .map_err(db_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_error)
    }

    pub fn active_passkey_by_credential(
        &self,
        credential_id: &[u8],
    ) -> OpenFangResult<Option<StoredPasskey>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT p.credential_id, p.passkey_json, u.id, u.slug, u.display_name
             FROM dashboard_auth_passkeys p JOIN dashboard_auth_users u ON u.id = p.user_id
             WHERE p.credential_id = ?1 AND p.revoked_at IS NULL",
            params![credential_id],
            |row| {
                Ok(StoredPasskey {
                    credential_id: row.get(0)?,
                    passkey_json: row.get(1)?,
                    user: DashboardUser {
                        id: row.get(2)?,
                        slug: row.get(3)?,
                        display_name: row.get(4)?,
                    },
                })
            },
        )
        .optional()
        .map_err(db_error)
    }

    pub fn update_passkey(
        &self,
        credential_id: &[u8],
        passkey_json: &str,
        now: i64,
    ) -> OpenFangResult<()> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE dashboard_auth_passkeys SET passkey_json = ?1, last_used_at = ?2 WHERE credential_id = ?3 AND revoked_at IS NULL",
                params![passkey_json, now, credential_id],
            )
            .map_err(db_error)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(memory_error("active passkey not found"))
        }
    }

    pub fn create_session(
        &self,
        token_hash: &[u8],
        user_id: &str,
        credential_id: &[u8],
        now: i64,
        expires_at: i64,
    ) -> OpenFangResult<()> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "INSERT INTO dashboard_auth_sessions (token_hash, user_id, credential_id, created_at, expires_at)
             SELECT ?1, ?2, ?3, ?4, ?5
             WHERE EXISTS (
                 SELECT 1 FROM dashboard_auth_passkeys
                 WHERE credential_id = ?3 AND user_id = ?2 AND revoked_at IS NULL
             )",
            params![token_hash, user_id, credential_id, now, expires_at],
        )
        .map_err(db_error)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(memory_error(
                "cannot create a session for a missing, revoked, or mismatched passkey",
            ))
        }
    }

    pub fn validate_session(
        &self,
        token_hash: &[u8],
        now: i64,
    ) -> OpenFangResult<Option<SessionPrincipal>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT u.id, u.slug, u.display_name, s.credential_id, s.expires_at
             FROM dashboard_auth_sessions s
             JOIN dashboard_auth_users u ON u.id = s.user_id
             JOIN dashboard_auth_passkeys p ON p.credential_id = s.credential_id AND p.user_id = s.user_id
             WHERE s.token_hash = ?1 AND s.revoked_at IS NULL AND s.expires_at > ?2 AND p.revoked_at IS NULL",
            params![token_hash, now],
            |row| {
                Ok(SessionPrincipal {
                    user: DashboardUser { id: row.get(0)?, slug: row.get(1)?, display_name: row.get(2)? },
                    credential_id: row.get(3)?,
                    expires_at: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(db_error)
    }

    pub fn revoke_session(&self, token_hash: &[u8], now: i64) -> OpenFangResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE dashboard_auth_sessions SET revoked_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL",
            params![now, token_hash],
        )
        .map_err(db_error)?;
        Ok(())
    }

    pub fn revoke_slot(&self, slug: &str, now: i64) -> OpenFangResult<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_error)?;
        let Some(user) = load_user_by_slug(&tx, slug)? else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE dashboard_auth_passkeys SET revoked_at = ?1 WHERE user_id = ?2 AND revoked_at IS NULL",
            params![now, user.id],
        ).map_err(db_error)?;
        tx.execute(
            "UPDATE dashboard_auth_sessions SET revoked_at = ?1 WHERE user_id = ?2 AND revoked_at IS NULL",
            params![now, user.id],
        ).map_err(db_error)?;
        tx.execute(
            "UPDATE dashboard_auth_invites SET revoked_at = ?1 WHERE user_id = ?2 AND consumed_at IS NULL AND revoked_at IS NULL",
            params![now, user.id],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(true)
    }

    /// Permanently delete a revoked slot and all of its authentication records.
    ///
    /// This is intended for ephemeral smoke-test identities. Normal user slots
    /// should use `revoke_slot` so their audit history remains available.
    pub fn delete_slot(&self, slug: &str) -> OpenFangResult<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(db_error)?;
        let Some(user) = load_user_by_slug(&tx, slug)? else {
            return Ok(false);
        };
        tx.execute(
            "DELETE FROM dashboard_auth_sessions WHERE user_id = ?1",
            params![user.id],
        )
        .map_err(db_error)?;
        tx.execute(
            "DELETE FROM dashboard_auth_invites WHERE user_id = ?1",
            params![user.id],
        )
        .map_err(db_error)?;
        tx.execute(
            "DELETE FROM dashboard_auth_passkeys WHERE user_id = ?1",
            params![user.id],
        )
        .map_err(db_error)?;
        tx.execute(
            "DELETE FROM dashboard_auth_users WHERE id = ?1",
            params![user.id],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(true)
    }

    pub fn list_slots(&self, now: i64) -> OpenFangResult<Vec<SlotStatus>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT u.slug, u.display_name,
                    (SELECT COUNT(*) FROM dashboard_auth_passkeys p WHERE p.user_id = u.id AND p.revoked_at IS NULL),
                    (SELECT MAX(i.expires_at) FROM dashboard_auth_invites i WHERE i.user_id = u.id AND i.consumed_at IS NULL AND i.revoked_at IS NULL AND i.expires_at > ?1)
             FROM dashboard_auth_users u ORDER BY u.slug"
        ).map_err(db_error)?;
        let rows = stmt
            .query_map(params![now], |row| {
                Ok(SlotStatus {
                    slug: row.get(0)?,
                    display_name: row.get(1)?,
                    active_credentials: row.get::<_, i64>(2)? as u64,
                    pending_invite_expires_at: row.get(3)?,
                })
            })
            .map_err(db_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_error)
    }

    fn lock(&self) -> OpenFangResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| memory_error("dashboard auth database lock poisoned"))
    }
}

fn load_user_by_slug(conn: &Connection, slug: &str) -> OpenFangResult<Option<DashboardUser>> {
    conn.query_row(
        "SELECT id, slug, display_name FROM dashboard_auth_users WHERE slug = ?1",
        params![slug],
        |row| {
            Ok(DashboardUser {
                id: row.get(0)?,
                slug: row.get(1)?,
                display_name: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(db_error)
}

fn load_user_by_id(conn: &Connection, id: &str) -> OpenFangResult<Option<DashboardUser>> {
    conn.query_row(
        "SELECT id, slug, display_name FROM dashboard_auth_users WHERE id = ?1",
        params![id],
        |row| {
            Ok(DashboardUser {
                id: row.get(0)?,
                slug: row.get(1)?,
                display_name: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(db_error)
}

fn validate_slug(slug: &str) -> OpenFangResult<()> {
    if slug.is_empty()
        || slug.len() > 64
        || !slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(memory_error(
            "slot slug must contain only lowercase ASCII letters, digits, and hyphens",
        ));
    }
    Ok(())
}

fn db_error(error: rusqlite::Error) -> OpenFangError {
    memory_error(error.to_string())
}

fn memory_error(message: impl Into<String>) -> OpenFangError {
    OpenFangError::Memory(message.into())
}

#[cfg(test)]
mod tests {
    use crate::MemorySubstrate;

    fn store() -> MemorySubstrate {
        MemorySubstrate::open_in_memory(0.1).unwrap()
    }

    #[test]
    fn invite_is_single_use_and_session_follows_credential_revocation() {
        let memory = store();
        let auth = memory.dashboard_auth();
        let token_hash = [7u8; 32];
        let invite = auth
            .create_invite("alice", "Alice", &token_hash, 100, 200)
            .unwrap();
        assert!(auth.find_valid_invite(&token_hash, 150).unwrap().is_some());
        let user = auth
            .consume_invite_and_store_passkey(&invite.id, b"credential", "{}", 160)
            .unwrap();
        assert_eq!(user.slug, "alice");
        assert!(auth.find_valid_invite(&token_hash, 161).unwrap().is_none());
        assert!(auth
            .consume_invite_and_store_passkey(&invite.id, b"other", "{}", 162)
            .is_err());

        auth.create_session(b"session", &user.id, b"credential", 170, 300)
            .unwrap();
        assert!(auth.validate_session(b"session", 200).unwrap().is_some());
        assert!(auth.revoke_slot("alice", 210).unwrap());
        assert!(auth.validate_session(b"session", 211).unwrap().is_none());
    }

    #[test]
    fn replacing_pending_invite_revokes_the_old_one() {
        let memory = store();
        let auth = memory.dashboard_auth();
        auth.create_invite("reserve", "Reserve", b"one", 100, 200)
            .unwrap();
        auth.create_invite("reserve", "Reserve", b"two", 110, 210)
            .unwrap();
        assert!(auth.find_valid_invite(b"one", 120).unwrap().is_none());
        assert!(auth.find_valid_invite(b"two", 120).unwrap().is_some());
    }

    #[test]
    fn deleting_an_ephemeral_slot_removes_all_authentication_records() {
        let memory = store();
        let auth = memory.dashboard_auth();
        let invite = auth
            .create_invite("smoke", "Smoke test", b"invite", 100, 200)
            .unwrap();
        let user = auth
            .consume_invite_and_store_passkey(&invite.id, b"credential", "{}", 110)
            .unwrap();
        auth.create_session(b"session", &user.id, b"credential", 120, 300)
            .unwrap();

        assert!(auth.revoke_slot("smoke", 130).unwrap());
        assert!(auth.delete_slot("smoke").unwrap());
        assert!(auth.list_slots(140).unwrap().is_empty());
        assert!(auth.validate_session(b"session", 140).unwrap().is_none());
        assert!(!auth.delete_slot("smoke").unwrap());
    }
}
