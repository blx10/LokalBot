use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::models::Models;
use crate::{BackgroundItem, BackgroundKind, EngineEvent, Shared};

const BACKGROUND_ID: &str = "reindex";

fn semantic_indexing_allowed(settings: &lokalbot_core::Settings, missing_embeddings: bool) -> bool {
    settings.semantic_search_enabled
        && !crate::post_correction::local_llama_server_disabled(settings)
        && !missing_embeddings
}

pub fn start(shared: Arc<Shared>, models: Models) {
    let spawned = std::thread::Builder::new()
        .name("lokalbot-index".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            match shared.library.reindex_stale() {
                Ok(0) => {}
                Ok(count) => {
                    log::info!("indexed {count} meetings for search");
                    shared.emit(EngineEvent::LibraryChanged);
                }
                Err(err) => log::warn!("updating the search index failed: {err:#}"),
            }
            while models.runtime_updating() && !shared.is_shutting_down() {
                std::thread::sleep(Duration::from_secs(1));
            }
            let settings = shared.settings();
            if !semantic_indexing_allowed(
                &settings,
                !models.missing(&models.embedding_requirements()).is_empty(),
            ) {
                return;
            }
            while shared.capture_active.load(Ordering::Relaxed) {
                if shared.is_shutting_down() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(10));
            }
            let result = lokalbot_ai::embeddings::index_pending(
                &shared.runtime,
                &shared.library,
                &shared.shutting_down,
                &mut |done, total| {
                    shared.set_background(BackgroundItem {
                        id: BACKGROUND_ID.into(),
                        kind: BackgroundKind::Reindex,
                        title: "Updating meaning search".into(),
                        detail: format!("{done} of {total} meetings"),
                        progress: Some(done as f32 / total.max(1) as f32),
                    });
                },
            );
            shared.clear_background(BACKGROUND_ID);
            match result {
                Ok(0) => {}
                Ok(count) => log::info!("embedded {count} meetings for meaning search"),
                Err(err) if shared.is_shutting_down() => log::debug!("indexing stopped: {err:#}"),
                Err(err) => log::warn!("updating meaning search failed: {err:#}"),
            }
        });
    if let Err(err) = spawned {
        log::warn!("could not start search indexing: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::semantic_indexing_allowed;

    #[test]
    fn semantic_indexing_skips_when_local_llama_server_disabled() {
        let mut settings = lokalbot_core::Settings {
            semantic_search_enabled: true,
            ..Default::default()
        };
        assert!(semantic_indexing_allowed(&settings, false));
        assert!(!semantic_indexing_allowed(&settings, true));

        crate::post_correction::set_local_llama_server_disabled(&mut settings, true);
        assert!(!semantic_indexing_allowed(&settings, false));

        crate::post_correction::set_local_llama_server_disabled(&mut settings, false);
        settings.other.insert(
            "think".into(),
            serde_json::json!({ "disable_local_llama_server": true }),
        );
        assert!(!semantic_indexing_allowed(&settings, false));
    }
}
