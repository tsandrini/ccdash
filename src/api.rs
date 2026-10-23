//! The API's TypeScript types for the panel, generated from the Rust
//! models with ts-rs into `web/src/lib/api/types.ts`. The test fails
//! while the committed file is stale; `CCDASH_UPDATE_TYPES=1 cargo test
//! api` rewrites it.

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ts_rs::{Config, TS};

    const FILE: &str = "web/src/lib/api/types.ts";

    fn generate(dir: &Path) {
        let cfg = Config::new().with_out_dir(dir).with_large_int("number");
        crate::model::Snapshot::export_all(&cfg).unwrap();
        crate::model::Update::export_all(&cfg).unwrap();
        crate::ask::Answer::export_all(&cfg).unwrap();
        crate::conversation::Conversation::export_all(&cfg).unwrap();
        crate::conversation::Stats::export_all(&cfg).unwrap();
        crate::conversation::Prompt::export_all(&cfg).unwrap();
        crate::knowledge::Note::export_all(&cfg).unwrap();
        crate::knowledge::NoteView::export_all(&cfg).unwrap();
        crate::knowledge::Hit::export_all(&cfg).unwrap();
        crate::knowledge::Written::export_all(&cfg).unwrap();
        crate::knowledge::NoteOp::export_all(&cfg).unwrap();
        crate::knowledge::Upsert::export_all(&cfg).unwrap();
        crate::knowledge::Query::export_all(&cfg).unwrap();
        crate::knowledge::projects::Projects::export_all(&cfg).unwrap();
        crate::knowledge::projects::Graph::export_all(&cfg).unwrap();
        crate::history::History::export_all(&cfg).unwrap();
        crate::analytics::Analytics::export_all(&cfg).unwrap();
        crate::push::Subscribe::export_all(&cfg).unwrap();
        crate::push::Subscription::export_all(&cfg).unwrap();
        crate::push::Outcome::export_all(&cfg).unwrap();
        crate::tokens::Token::export_all(&cfg).unwrap();
        crate::hub::http::Rename::export_all(&cfg).unwrap();
        crate::hub::http::Reply::export_all(&cfg).unwrap();
        crate::hub::http::PutNote::export_all(&cfg).unwrap();
        crate::hub::http::Unsubscribe::export_all(&cfg).unwrap();
        crate::hub::http::Label::export_all(&cfg).unwrap();
        crate::hub::http::LaunchBody::export_all(&cfg).unwrap();
        crate::model::LaunchTarget::export_all(&cfg).unwrap();
        crate::sync::SyncView::export_all(&cfg).unwrap();
        crate::sync::SyncStats::export_all(&cfg).unwrap();
        crate::hub::http::Resolve::export_all(&cfg).unwrap();
    }

    #[test]
    fn types_ts_is_current() {
        let dir = std::env::temp_dir().join(format!("ccdash-types-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        generate(&dir);
        let generated = std::fs::read_to_string(dir.join("types.ts")).unwrap();
        let others: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "types.ts")
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            others.is_empty(),
            "types exported outside types.ts: {others:?}"
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FILE);
        if std::env::var_os("CCDASH_UPDATE_TYPES").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &generated).unwrap();
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == generated,
            "{FILE} is stale: run `CCDASH_UPDATE_TYPES=1 cargo test api`"
        );
    }
}
