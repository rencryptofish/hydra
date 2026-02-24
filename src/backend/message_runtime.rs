use std::collections::{HashMap, HashSet, VecDeque};

use crate::backend::state::{BackgroundRefreshState, ConversationBuffer};
use crate::logs::{ConversationEntry, GlobalStats, SessionStats};
use crate::models::DiffFile;
use crate::session::AgentType;

pub(crate) struct MessageRuntime {
    last_messages: HashMap<String, String>,
    session_stats: HashMap<String, SessionStats>,
    global_stats: GlobalStats,
    diff_files: Vec<DiffFile>,
    conversations: HashMap<String, ConversationBuffer>,
    bg: BackgroundRefreshState,
}

pub(crate) struct MessageTickResult {
    pub(crate) changed_sessions: Vec<String>,
}

impl MessageRuntime {
    pub(crate) fn new() -> Self {
        Self {
            last_messages: HashMap::new(),
            session_stats: HashMap::new(),
            global_stats: GlobalStats::default(),
            diff_files: Vec::new(),
            conversations: HashMap::new(),
            bg: BackgroundRefreshState::new(),
        }
    }

    pub(crate) fn last_messages(&self) -> &HashMap<String, String> {
        &self.last_messages
    }

    pub(crate) fn session_stats(&self) -> &HashMap<String, SessionStats> {
        &self.session_stats
    }

    pub(crate) fn global_stats(&self) -> &GlobalStats {
        &self.global_stats
    }

    pub(crate) fn diff_files(&self) -> &[DiffFile] {
        &self.diff_files
    }

    pub(crate) fn conversations(&self) -> &HashMap<String, ConversationBuffer> {
        &self.conversations
    }

    pub(crate) fn snapshot_conversations(&self) -> HashMap<String, VecDeque<ConversationEntry>> {
        self.conversations
            .iter()
            .map(|(tmux_name, buf)| (tmux_name.clone(), buf.entries.clone()))
            .collect()
    }

    pub(crate) fn tick(
        &mut self,
        sessions: &[(String, AgentType)],
        cwd: &str,
    ) -> Option<MessageTickResult> {
        let conversation_offsets: HashMap<String, u64> = self
            .conversations
            .iter()
            .map(|(tmux_name, buf)| (tmux_name.clone(), buf.read_offset))
            .collect();

        let result = self.bg.tick(
            sessions,
            &self.session_stats,
            &self.global_stats,
            cwd,
            conversation_offsets,
        )?;

        let changed_sessions: Vec<String> = result
            .conversation_offsets
            .iter()
            .filter_map(|(tmux_name, new_offset)| {
                let old_offset = self
                    .conversations
                    .get(tmux_name)
                    .map(|buf| buf.read_offset)
                    .unwrap_or(0);
                if *new_offset != old_offset {
                    Some(tmux_name.clone())
                } else {
                    None
                }
            })
            .collect();

        for tmux_name in &result.clear_last_messages {
            self.last_messages.remove(tmux_name);
        }
        self.last_messages.extend(result.last_messages);
        self.session_stats = result.session_stats;
        self.global_stats = result.global_stats;
        self.diff_files = result.diff_files;

        for (tmux_name, offset) in &result.conversation_offsets {
            let buf = self
                .conversations
                .entry(tmux_name.clone())
                .or_insert_with(ConversationBuffer::new);
            buf.read_offset = *offset;
        }

        let conversation_keys: HashSet<String> = result.conversations.keys().cloned().collect();

        for (tmux_name, new_entries) in result.conversations {
            let replace = result.conversation_replace.contains(&tmux_name);

            let buf = self
                .conversations
                .entry(tmux_name.clone())
                .or_insert_with(ConversationBuffer::new);
            if replace {
                buf.entries.clear();
            }
            buf.extend(new_entries);
        }

        for tmux_name in &result.conversation_replace {
            if conversation_keys.contains(tmux_name) {
                continue;
            }

            let buf = self
                .conversations
                .entry(tmux_name.clone())
                .or_insert_with(ConversationBuffer::new);
            buf.entries.clear();
        }

        Some(MessageTickResult { changed_sessions })
    }

    pub(crate) fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.last_messages.retain(|k, _| live_keys.contains(k));
        self.session_stats.retain(|k, _| live_keys.contains(k));
        self.conversations.retain(|k, _| live_keys.contains(k));
        self.bg.prune(live_keys);
    }
}
