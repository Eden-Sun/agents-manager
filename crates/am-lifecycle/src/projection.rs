//! Small projection value types shared by lifecycle and the daemon composition layer.

use crate::db;

#[derive(Debug, Default, Clone)]
pub struct Owned {
    bot_ids: std::collections::HashSet<String>,
    project_ids: std::collections::HashSet<String>,
    roles: std::collections::HashMap<String, &'static str>,
}

impl Owned {
    pub fn owns(&self, b: &db::Bot) -> bool {
        self.bot_ids.contains(&b.id)
            || b.parent_bot_id.as_ref().is_some_and(|p| self.bot_ids.contains(p))
            || self.project_ids.contains(&b.project_id)
    }

    pub fn role(&self, b: &db::Bot) -> &'static str {
        if let Some(r) = self.roles.get(&b.id) {
            r
        } else if b.parent_bot_id.as_ref().is_some_and(|p| self.bot_ids.contains(p)) {
            "AGM 開出去的子 agent"
        } else {
            "AGM 專案裡的常駐工人"
        }
    }

    pub fn add_bot(&mut self, id: String, role: &'static str) {
        self.roles.entry(id.clone()).or_insert(role);
        self.bot_ids.insert(id);
    }

    pub fn add_project(&mut self, id: String) {
        self.project_ids.insert(id);
    }

    pub fn owns_project_id(&self, id: &str) -> bool {
        self.project_ids.contains(id)
    }
}
