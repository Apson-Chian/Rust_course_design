//! 运行时内存键值数据管理。
//!
//! 使用 [`BTreeMap`] 作为底层结构：键天然有序，`LIST` 无需额外排序；
//! 同时支持可选的过期时间（扩展功能），采用「惰性删除」策略，
//! 即访问到已过期的键时才真正移除。

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// 返回当前 UNIX 毫秒时间戳
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一条键值记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub value: String,
    /// 绝对过期时间（UNIX 毫秒），`None` 表示永不过期。
    /// 使用绝对时间而非剩余时长，重启恢复后过期语义仍然正确。
    pub expire_at_ms: Option<u64>,
}

impl Entry {
    fn is_expired(&self, now: u64) -> bool {
        matches!(self.expire_at_ms, Some(t) if t <= now)
    }
}

/// 内存键值存储（不含持久化，不含并发控制）
#[derive(Debug, Default)]
pub struct Store {
    map: BTreeMap<String, Entry>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入或覆盖，`expire_at_ms` 为 `None` 表示永不过期
    pub fn set(&mut self, key: String, value: String, expire_at_ms: Option<u64>) {
        self.map.insert(
            key,
            Entry {
                value,
                expire_at_ms,
            },
        );
    }

    /// 查询；命中已过期的键时顺带删除并返回 `None`
    pub fn get(&mut self, key: &str) -> Option<&str> {
        let now = now_ms();
        if self.map.get(key).is_some_and(|e| e.is_expired(now)) {
            self.map.remove(key);
        }
        self.map.get(key).map(|e| e.value.as_str())
    }

    /// 删除，返回是否真的删掉了一个有效键
    pub fn remove(&mut self, key: &str) -> bool {
        match self.map.remove(key) {
            Some(e) => !e.is_expired(now_ms()),
            None => false,
        }
    }

    /// 列出全部有效键（升序）
    pub fn keys(&mut self) -> Vec<String> {
        self.purge_expired();
        self.map.keys().cloned().collect()
    }

    /// 有效键数量
    pub fn len(&mut self) -> usize {
        self.purge_expired();
        self.map.len()
    }

    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    /// 遍历全部有效记录，供日志压缩等场景使用
    pub fn iter_valid(&mut self) -> impl Iterator<Item = (&String, &Entry)> {
        self.purge_expired();
        self.map.iter()
    }

    /// 清理所有已过期的记录
    fn purge_expired(&mut self) {
        let now = now_ms();
        self.map.retain(|_, e| !e.is_expired(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_overwrite_remove() {
        let mut s = Store::new();
        assert_eq!(s.get("k"), None);

        s.set("k".into(), "v1".into(), None);
        assert_eq!(s.get("k"), Some("v1"));

        s.set("k".into(), "v2".into(), None); // 覆盖
        assert_eq!(s.get("k"), Some("v2"));

        assert!(s.remove("k"));
        assert!(!s.remove("k")); // 重复删除返回 false
        assert_eq!(s.get("k"), None);
    }

    #[test]
    fn keys_are_sorted_and_counted() {
        let mut s = Store::new();
        for k in ["b", "a", "c"] {
            s.set(k.into(), "v".into(), None);
        }
        assert_eq!(s.keys(), vec!["a", "b", "c"]);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn expired_entry_is_invisible() {
        let mut s = Store::new();
        s.set("gone".into(), "v".into(), Some(now_ms() - 1)); // 已过期
        s.set("alive".into(), "v".into(), Some(now_ms() + 60_000));

        assert_eq!(s.get("gone"), None);
        assert_eq!(s.get("alive"), Some("v"));
        assert_eq!(s.keys(), vec!["alive"]);
        assert_eq!(s.len(), 1);
    }
}
