use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ENTRIES: usize = 500;
const MAX_TEXT_BYTES: usize = 6 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub timestamp: i64,
    pub raw: String,
    pub text: String,
    pub model: String,
}

pub struct History {
    path: PathBuf,
    pub entries: Vec<Entry>,
}

impl History {
    pub fn load() -> Result<Self> {
        Self::at(crate::config::path()?.with_file_name("history.json"))
    }

    pub(crate) fn at(path: PathBuf) -> Result<Self> {
        let mut entries: Vec<Entry> = if path.exists() {
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 16 * 1024 * 1024, "历史记录文件过大");
            serde_json::from_slice(&bytes).context("历史记录文件损坏，原文件未修改")?
        } else {
            Vec::new()
        };
        Self::prune(&mut entries);
        Ok(Self { path, entries })
    }

    fn prune(entries: &mut Vec<Entry>) {
        entries.truncate(MAX_ENTRIES);
        let mut bytes = 0;
        let keep = entries
            .iter()
            .take_while(|entry| {
                bytes += entry.raw.len() + entry.text.len();
                bytes <= MAX_TEXT_BYTES
            })
            .count();
        entries.truncate(keep);
    }

    pub fn append(&mut self, raw: String, text: String, model: String) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        let id = now.max(self.entries.first().map_or(0, |e| e.id.saturating_add(1)));
        let entry = Entry {
            id,
            timestamp: (now / 1000) as i64,
            raw,
            text,
            model,
        };
        self.edit(|entries| entries.insert(0, entry))
    }

    pub fn remove(&mut self, id: u64) -> Result<()> {
        self.edit(|entries| entries.retain(|entry| entry.id != id))
    }

    pub fn clear(&mut self) -> Result<()> {
        self.edit(Vec::clear)
    }

    fn edit(&mut self, change: impl FnOnce(&mut Vec<Entry>)) -> Result<()> {
        let mut next = self.entries.clone();
        change(&mut next);
        Self::prune(&mut next);
        std::fs::create_dir_all(self.path.parent().context("历史记录目录无效")?)?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec(&next)?)?;
        crate::platform::replace_file(&temp, &self.path).context("保存历史记录失败")?;
        self.entries = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_survives_restart_and_retains_unique_ids() {
        let dir = std::env::temp_dir().join(format!(
            "asr-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("history.json");
        let mut history = History::at(path.clone()).unwrap();
        history
            .append("原文".into(), "润色".into(), "test".into())
            .unwrap();
        history
            .append("第二条".into(), "第二条".into(), "test".into())
            .unwrap();
        let mut reloaded = History::at(path).unwrap();
        assert_eq!(reloaded.entries[0].text, "第二条");
        assert!(reloaded.entries[0].id > reloaded.entries[1].id);
        reloaded.remove(reloaded.entries[0].id).unwrap();
        assert_eq!(reloaded.entries[0].raw, "原文");
        reloaded.clear().unwrap();
        assert!(
            History::at(reloaded.path.clone())
                .unwrap()
                .entries
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_save_keeps_in_memory_history() {
        let parent =
            std::env::temp_dir().join(format!("asr-history-invalid-parent-{}", std::process::id()));
        std::fs::write(&parent, b"not a directory").unwrap();
        let mut history = History {
            path: parent.join("history.json"),
            entries: Vec::new(),
        };
        assert!(
            history
                .append("raw".into(), "text".into(), "model".into())
                .is_err()
        );
        assert!(history.entries.is_empty());
        std::fs::remove_file(parent).unwrap();
    }
}
