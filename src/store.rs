//! Account-scoped documents and search, plus a durable journal that prevents duplicate writes.

use std::{path::Path, time::Duration};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::{Home, private_write},
    content::Document,
    error::{Error, Result},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct CachedDocument {
    pub reference: String,
    pub course_id: u64,
    pub module_id: u64,
    pub title: String,
    pub url: String,
    pub filename: String,
    pub mime: String,
    pub text: String,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub observed_at: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(home: &Home) -> Result<Self> {
        home.ensure()?;
        let path = home.path.join("moocp.sqlite3");
        if !path.exists() {
            private_write(&path, b"")?;
        }
        Self::at(&path)
    }
    fn at(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS documents (
                profile TEXT NOT NULL, reference TEXT NOT NULL, course_id INTEGER NOT NULL,
                module_id INTEGER NOT NULL, title TEXT NOT NULL, url TEXT NOT NULL,
                filename TEXT NOT NULL, mime TEXT NOT NULL, text TEXT NOT NULL,
                warnings TEXT NOT NULL, truncated INTEGER NOT NULL, observed_at TEXT NOT NULL,
                bytes BLOB NOT NULL, UNIQUE(profile, reference));
            CREATE VIRTUAL TABLE IF NOT EXISTS document_search USING fts5(title, text, content='documents', content_rowid='rowid');
            CREATE TRIGGER IF NOT EXISTS doc_insert AFTER INSERT ON documents BEGIN
                INSERT INTO document_search(rowid,title,text) VALUES(new.rowid,new.title,new.text); END;
            CREATE TRIGGER IF NOT EXISTS doc_delete AFTER DELETE ON documents BEGIN
                INSERT INTO document_search(document_search,rowid,title,text) VALUES('delete',old.rowid,old.title,old.text); END;
            CREATE TRIGGER IF NOT EXISTS doc_update AFTER UPDATE ON documents BEGIN
                INSERT INTO document_search(document_search,rowid,title,text) VALUES('delete',old.rowid,old.title,old.text);
                INSERT INTO document_search(rowid,title,text) VALUES(new.rowid,new.title,new.text); END;
            CREATE TABLE IF NOT EXISTS proposals (
                id TEXT PRIMARY KEY, profile TEXT NOT NULL, payload TEXT NOT NULL,
                preview TEXT NOT NULL, preconditions TEXT NOT NULL, status TEXT NOT NULL,
                created_at TEXT NOT NULL, expires_at TEXT NOT NULL, receipt TEXT);
            PRAGMA user_version=1;")?;
        Ok(Self { db })
    }
    pub fn cache(&self, profile: &str, document: &CachedDocument) -> Result<()> {
        let course_id = i64::try_from(document.course_id)
            .map_err(|_| Error::invalid("Course ID exceeds SQLite's integer range."))?;
        let module_id = i64::try_from(document.module_id)
            .map_err(|_| Error::invalid("Module ID exceeds SQLite's integer range."))?;
        self.db.execute("INSERT INTO documents (profile,reference,course_id,module_id,title,url,filename,mime,text,warnings,truncated,observed_at,bytes)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
            ON CONFLICT(profile,reference) DO UPDATE SET title=excluded.title,url=excluded.url,filename=excluded.filename,
            mime=excluded.mime,text=excluded.text,warnings=excluded.warnings,truncated=excluded.truncated,observed_at=excluded.observed_at,bytes=excluded.bytes",
            params![profile,document.reference,course_id,module_id,document.title,document.url,document.filename,document.mime,document.text,serde_json::to_string(&document.warnings)?,document.truncated,document.observed_at,document.bytes])?;
        Ok(())
    }
    pub fn document(&self, profile: &str, reference: &str) -> Result<Option<CachedDocument>> {
        Ok(self.db.query_row("SELECT reference,course_id,module_id,title,url,filename,mime,text,warnings,truncated,observed_at,bytes FROM documents WHERE profile=?1 AND reference=?2", params![profile,reference], |row| {
            Ok(CachedDocument { reference:row.get(0)?,course_id:row.get::<_,i64>(1)? as u64,module_id:row.get::<_,i64>(2)? as u64,title:row.get(3)?,url:row.get(4)?,filename:row.get(5)?,mime:row.get(6)?,text:row.get(7)?,warnings:serde_json::from_str(&row.get::<_,String>(8)?).unwrap_or_default(),truncated:row.get(9)?,observed_at:row.get(10)?,bytes:row.get(11)? })
        }).optional()?)
    }
    pub fn search(
        &self,
        profile: &str,
        query: &str,
        course: Option<u64>,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        if query.chars().count() > 500 || !(1..=50).contains(&limit) {
            return Err(Error::invalid(
                "Use a query up to 500 characters and a limit between 1 and 50.",
            ));
        }
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .take(25)
            .map(|s| format!("\"{s}\""))
            .collect();
        if words.is_empty() {
            return Err(Error::invalid("Search requires at least one word."));
        }
        let expression = words.join(" AND ");
        let course = course
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::invalid("Course ID is too large."))?;
        let offset_sql =
            i64::try_from(offset).map_err(|_| Error::invalid("Search offset is too large."))?;
        let mut statement = self.db.prepare(
            "SELECT d.reference,d.course_id,d.module_id,d.title,d.url,d.observed_at,
            snippet(document_search,1,'[',']',' … ',40),bm25(document_search,5.0,1.0)
            FROM document_search JOIN documents d ON d.rowid=document_search.rowid
            WHERE document_search MATCH ?1 AND d.profile=?2 AND (?3 IS NULL OR d.course_id=?3)
            ORDER BY bm25(document_search,5.0,1.0),d.reference LIMIT ?4 OFFSET ?5",
        )?;
        let rows = statement.query_map(params![expression,profile,course,(limit+1) as i64,offset_sql], |row| {
            Ok(json!({"reference":row.get::<_,String>(0)?,"course_id":row.get::<_,i64>(1)? as u64,"module_id":row.get::<_,i64>(2)? as u64,"title":row.get::<_,String>(3)?,"source_url":row.get::<_,String>(4)?,"observed_at":row.get::<_,String>(5)?,"passage":row.get::<_,String>(6)?}))
        })?;
        let mut results = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = results.len() > limit;
        results.truncate(limit);
        Ok(
            json!({"results":results,"next_offset":if has_more {Some(offset+limit)} else {None},"coverage":self.coverage(profile)?,"search_type":"lexical","scope":"Only successfully extracted local documents; use moodle_index_course to expand coverage."}),
        )
    }
    pub fn coverage(&self, profile: &str) -> Result<Value> {
        let mut statement = self.db.prepare("SELECT course_id,COUNT(*),MIN(observed_at),MAX(observed_at) FROM documents WHERE profile=?1 GROUP BY course_id ORDER BY course_id")?;
        let rows = statement.query_map([profile], |r| Ok(json!({"course_id":r.get::<_,i64>(0)? as u64,"documents":r.get::<_,i64>(1)? as u64,"oldest_observation":r.get::<_,String>(2)?,"newest_observation":r.get::<_,String>(3)?})))?;
        Ok(json!(rows.collect::<std::result::Result<Vec<_>, _>>()?))
    }
    pub fn clear_profile(&self, profile: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM documents WHERE profile=?1", [profile])?;
        self.db
            .execute("DELETE FROM proposals WHERE profile=?1", [profile])?;
        Ok(())
    }
    pub fn prepare(
        &self,
        profile: &str,
        payload: &Value,
        preview: &Value,
        preconditions: &Value,
    ) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let expires = now + chrono::Duration::minutes(30);
        self.db.execute("INSERT INTO proposals(id,profile,payload,preview,preconditions,status,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,'prepared',?6,?7)", params![id,profile,serde_json::to_string(payload)?,serde_json::to_string(preview)?,serde_json::to_string(preconditions)?,now.to_rfc3339(),expires.to_rfc3339()])?;
        Ok(
            json!({"proposal_id":id,"preview":preview,"expires_at":expires.to_rfc3339(),"status":"prepared","remote_changes":false,"review_command":format!("moocp review {id}"),"next_step":"Call moodle_execute_action with this proposal_id to request an in-client review. If the client has no form elicitation support, the student can review it in their own terminal with the review_command."}),
        )
    }
    pub fn proposal(&self, id: &str) -> Result<Value> {
        let result = self.db.query_row("SELECT profile,payload,preview,preconditions,status,created_at,expires_at,receipt FROM proposals WHERE id=?1", [id], |row| {
            let parse = |i| -> rusqlite::Result<Value> { let s:String=row.get(i)?; Ok(serde_json::from_str(&s).unwrap_or(Value::Null)) };
            let receipt: Option<String> = row.get(7)?;
            Ok(json!({"id":id,"profile":row.get::<_,String>(0)?,"payload":parse(1)?,"preview":parse(2)?,"preconditions":parse(3)?,"status":row.get::<_,String>(4)?,"created_at":row.get::<_,String>(5)?,"expires_at":row.get::<_,String>(6)?,"receipt":receipt.and_then(|s|serde_json::from_str::<Value>(&s).ok())}))
        }).optional()?;
        result.ok_or_else(|| {
            Error::new(
                "proposal_not_found",
                "No action proposal with that identifier.",
            )
        })
    }
    pub fn approve(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let changed = self.db.execute("UPDATE proposals SET status='approved' WHERE id=?1 AND status='prepared' AND expires_at>?2", params![id,now])?;
        if changed != 1 {
            return Err(Error::new(
                "proposal_state",
                "Proposal is expired or no longer awaiting review.",
            ));
        }
        Ok(())
    }
    pub fn claim(&self, profile: &str, id: &str) -> Result<Value> {
        let now = chrono::Utc::now().to_rfc3339();
        let changed = self.db.execute("UPDATE proposals SET status='executing' WHERE id=?1 AND profile=?2 AND status='approved' AND expires_at>?3", params![id,profile,now])?;
        if changed != 1 {
            let p = self.proposal(id)?;
            return Err(Error::new(
                "action_not_executable",
                format!(
                    "Proposal state is {}. It must be reviewed and approved, unexpired, and belong to this account. An executing/uncertain action is never automatically retried.",
                    p["status"].as_str().unwrap_or("unknown")
                ),
            ));
        }
        self.proposal(id)
    }
    pub fn finish(&self, id: &str, status: &str, receipt: &Value) -> Result<()> {
        if !matches!(status, "succeeded" | "uncertain" | "failed" | "rejected") {
            return Err(Error::invalid("Invalid action status."));
        }
        let allowed = if status == "rejected" {
            "prepared"
        } else {
            "executing"
        };
        let changed = self.db.execute(
            "UPDATE proposals SET status=?2,receipt=?3 WHERE id=?1 AND status=?4",
            params![id, status, serde_json::to_string(receipt)?, allowed],
        )?;
        if changed != 1 {
            return Err(Error::new(
                "proposal_state",
                "This action is no longer in a state that can be finalised this way.",
            ));
        }
        Ok(())
    }
}

impl CachedDocument {
    pub fn extracted(
        reference: String,
        course_id: u64,
        module_id: u64,
        title: String,
        response: crate::network::Response,
        document: Document,
    ) -> Self {
        Self {
            reference,
            course_id,
            module_id,
            title,
            url: crate::content::safe_url(&response.url),
            filename: response.filename,
            mime: response.content_type,
            text: document.text,
            warnings: document.warnings,
            truncated: document.truncated,
            observed_at: chrono::Utc::now().to_rfc3339(),
            bytes: response.body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn action_claim_is_single_use_and_account_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(&directory.path().join("test.db")).unwrap();
        let proposal = store
            .prepare("alice", &json!({"kind":"message"}), &json!({}), &json!({}))
            .unwrap();
        let id = proposal["proposal_id"].as_str().unwrap();
        assert!(store.claim("alice", id).is_err());
        store.approve(id).unwrap();
        assert!(store.claim("bob", id).is_err());
        assert!(store.claim("alice", id).is_ok());
        assert!(store.claim("alice", id).is_err());
        store
            .finish(id, "succeeded", &json!({"status":"succeeded"}))
            .unwrap();
        assert!(store.finish(id, "rejected", &json!({})).is_err());
        assert_eq!(store.proposal(id).unwrap()["status"], "succeeded");
    }
    #[test]
    fn search_is_account_scoped_and_updates_index() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(&directory.path().join("test.db")).unwrap();
        let mut document = CachedDocument {
            reference: "cm:7".into(),
            course_id: 42,
            module_id: 7,
            title: "Lecture".into(),
            url: "https://example/".into(),
            filename: "page.html".into(),
            mime: "text/html".into(),
            text: "Neural networks".into(),
            warnings: vec![],
            truncated: false,
            observed_at: chrono::Utc::now().to_rfc3339(),
            bytes: vec![],
        };
        store.cache("alice", &document).unwrap();
        assert_eq!(
            store.search("alice", "neural", None, 10, 0).unwrap()["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            store.search("bob", "neural", None, 10, 0).unwrap()["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        document.text = "Linear algebra".into();
        store.cache("alice", &document).unwrap();
        assert!(
            store.search("alice", "neural", None, 10, 0).unwrap()["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
