use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{path::Path, sync::mpsc, thread};
use tokio::sync::oneshot;
type Work = Box<dyn FnOnce(&mut Connection) + Send>;
#[derive(Clone)]
pub struct Database(mpsc::Sender<Work>);
impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let c = Connection::open(path).context("无法打开历史数据库")?;
        let version: i32 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(version <= 1, "数据库来自更新版本，已停止写入以保护数据");
        c.busy_timeout(std::time::Duration::from_secs(3))?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
    CREATE TABLE IF NOT EXISTS clipboard_history(
      id INTEGER PRIMARY KEY AUTOINCREMENT,content TEXT NOT NULL,content_hash TEXT NOT NULL,
      comparison_hash TEXT NOT NULL,preview TEXT NOT NULL,byte_len INTEGER NOT NULL,
      char_count INTEGER NOT NULL,source TEXT NOT NULL,created_at_ms INTEGER NOT NULL,
      last_copied_at_ms INTEGER,is_pinned INTEGER NOT NULL DEFAULT 0);
    CREATE INDEX IF NOT EXISTS history_order ON clipboard_history(is_pinned DESC,id DESC);
    PRAGMA user_version=1;",
        )?;
        let (tx, rx) = mpsc::channel::<Work>();
        thread::Builder::new()
            .name("swiftclip-database".into())
            .spawn(move || {
                let mut c = c;
                for f in rx {
                    f(&mut c);
                }
            })?;
        Ok(Self(tx))
    }
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| anyhow::anyhow!("数据库线程已停止"))?;
        rx.await.context("数据库响应已中断")?
    }
    pub async fn add(&self, text: String, source: &str, pinned: bool, s: Settings) -> Result<i64> {
        let source = source.to_string();
        self.call(move|c|{
   let tx=c.transaction()?;
   tx.execute("INSERT INTO clipboard_history(content,content_hash,comparison_hash,preview,byte_len,char_count,source,created_at_ms,is_pinned)VALUES(?,?,?,?,?,?,?,?,?)",
    params![text,hash(&text),key(&text),text.chars().take(512).collect::<String>(),text.len(),text.chars().count(),source,now(),pinned])?;
   let id=tx.last_insert_rowid();prune(&tx,&s)?;tx.commit()?;Ok(id)
  }).await
    }
    pub async fn query(
        &self,
        q: String,
        pinned: bool,
        limit: usize,
    ) -> Result<(Vec<Row>, usize, usize)> {
        self.call(move|c|{
   let total=c.query_row("SELECT count(*) FROM clipboard_history",[],|r|r.get::<_,usize>(0))?;
   let matched=c.query_row("SELECT count(*) FROM clipboard_history WHERE instr(lower(content),lower(?1))>0 AND (?2=0 OR is_pinned=1)",params![q,pinned],|r|r.get::<_,usize>(0))?;
   let mut stmt=c.prepare("SELECT id,preview,source,created_at_ms,is_pinned,char_count,byte_len FROM clipboard_history WHERE instr(lower(content),lower(?1))>0 AND (?2=0 OR is_pinned=1) ORDER BY is_pinned DESC,id DESC LIMIT ?3")?;
   let rows=stmt.query_map(params![q,pinned,limit],row)?.collect::<rusqlite::Result<Vec<_>>>()?;
   Ok((rows,total,matched))
  }).await
    }
    pub async fn record(&self, id: i64) -> Result<Record> {
        self.call(move|c|Ok(c.query_row("SELECT id,preview,source,created_at_ms,is_pinned,char_count,byte_len,content FROM clipboard_history WHERE id=?",params![id],|r|Ok(Record{row:row(r)?,content:r.get(7)?}))?)).await
    }
    pub async fn latest(&self) -> Result<Option<String>> {
        self.call(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT content FROM clipboard_history ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
    }
    pub async fn pin(&self, id: i64, p: bool) -> Result<()> {
        self.call(move |c| {
            c.execute(
                "UPDATE clipboard_history SET is_pinned=? WHERE id=?",
                params![p, id],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn delete(&self, id: i64) -> Result<()> {
        self.call(move |c| {
            c.execute("DELETE FROM clipboard_history WHERE id=?", params![id])?;
            Ok(())
        })
        .await
    }
    pub async fn copied(&self, id: i64) -> Result<()> {
        self.call(move |c| {
            c.execute(
                "UPDATE clipboard_history SET last_copied_at_ms=? WHERE id=?",
                params![now(), id],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn clear(&self, all: bool) -> Result<()> {
        self.call(move |c| {
            c.execute(
                if all {
                    "DELETE FROM clipboard_history"
                } else {
                    "DELETE FROM clipboard_history WHERE is_pinned=0"
                },
                [],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn restore(&self, r: Record, s: Settings) -> Result<()> {
        self.call(move|c|{
  let tx=c.transaction()?;tx.execute("INSERT OR IGNORE INTO clipboard_history(id,content,content_hash,comparison_hash,preview,byte_len,char_count,source,created_at_ms,is_pinned)VALUES(?,?,?,?,?,?,?,?,?,?)",params![r.row.id,r.content,hash(&r.content),key(&r.content),r.row.preview,r.row.bytes,r.row.chars,r.row.source,r.row.created,r.row.pinned])?;
  prune(&tx,&s)?;tx.commit()?;Ok(())
 }).await
    }
    pub async fn clean(&self, s: Settings) -> Result<()> {
        self.call(move |c| {
            prune(c, &s)?;
            Ok(())
        })
        .await
    }
}
fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        preview: r.get(1)?,
        source: r.get(2)?,
        created: r.get(3)?,
        pinned: r.get(4)?,
        chars: r.get(5)?,
        bytes: r.get(6)?,
    })
}
fn prune(c: &Connection, s: &Settings) -> Result<()> {
    if s.retention_days > 0 {
        c.execute(
            "DELETE FROM clipboard_history WHERE is_pinned=0 AND created_at_ms<?",
            params![now() - s.retention_days.min(36500) as i64 * 86400000],
        )?;
    }
    if s.history_limit > 0 {
        let pins: usize = c.query_row(
            "SELECT count(*) FROM clipboard_history WHERE is_pinned=1",
            [],
            |r| r.get(0),
        )?;
        c.execute("DELETE FROM clipboard_history WHERE id IN(SELECT id FROM clipboard_history WHERE is_pinned=0 ORDER BY id DESC LIMIT -1 OFFSET ?)",params![s.history_limit.saturating_sub(pins)])?;
    }
    Ok(())
}
