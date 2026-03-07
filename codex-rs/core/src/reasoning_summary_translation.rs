use crate::config::Config;
use crate::config::TranslationConfig;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingReasoningSummarySegment {
    pub(crate) summary_index: i64,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReasoningSummaryTranslationState {
    config: Option<TranslationConfig>,
    pending_segments: HashMap<String, PendingReasoningSummarySegment>,
    translated_segments: HashMap<String, Vec<String>>,
}

impl ReasoningSummaryTranslationState {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            config: config.translation.clone(),
            pending_segments: HashMap::new(),
            translated_segments: HashMap::new(),
        }
    }

    pub(crate) fn config(&self) -> Option<&TranslationConfig> {
        self.config.as_ref()
    }

    pub(crate) fn push_delta(
        &mut self,
        item_id: &str,
        summary_index: i64,
        delta: &str,
    ) -> Option<PendingReasoningSummarySegment> {
        self.config.as_ref()?;

        let entry = self
            .pending_segments
            .entry(item_id.to_string())
            .or_insert_with(|| PendingReasoningSummarySegment {
                summary_index,
                text: String::new(),
            });
        if entry.summary_index != summary_index {
            let completed = std::mem::replace(
                entry,
                PendingReasoningSummarySegment {
                    summary_index,
                    text: delta.to_string(),
                },
            );
            if completed.text.is_empty() {
                None
            } else {
                Some(completed)
            }
        } else {
            entry.text.push_str(delta);
            None
        }
    }

    pub(crate) fn start_new_section(
        &mut self,
        item_id: &str,
        summary_index: i64,
    ) -> Option<PendingReasoningSummarySegment> {
        self.config.as_ref()?;

        let previous = self.pending_segments.insert(
            item_id.to_string(),
            PendingReasoningSummarySegment {
                summary_index,
                text: String::new(),
            },
        );
        previous.filter(|segment| !segment.text.is_empty())
    }

    pub(crate) fn finish_item(&mut self, item_id: &str) -> Option<PendingReasoningSummarySegment> {
        self.config.as_ref()?;

        self.pending_segments
            .remove(item_id)
            .filter(|segment| !segment.text.is_empty())
    }

    pub(crate) fn record_translated_segment(&mut self, item_id: &str, translated: String) {
        self.translated_segments
            .entry(item_id.to_string())
            .or_default()
            .push(translated);
    }

    pub(crate) fn take_translated_segments(&mut self, item_id: &str) -> Option<Vec<String>> {
        self.translated_segments.remove(item_id)
    }

    pub(crate) fn replace_item_summary(item: &mut ResponseItem, segments: Vec<String>) -> bool {
        let ResponseItem::Reasoning { summary, .. } = item else {
            return false;
        };
        if summary.len() != segments.len() {
            return false;
        }

        *summary = segments
            .into_iter()
            .map(|text| ReasoningItemReasoningSummary::SummaryText { text })
            .collect();
        true
    }
}
