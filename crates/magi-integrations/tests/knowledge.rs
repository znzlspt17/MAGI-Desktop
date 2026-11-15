use magi_integrations::*;
#[test]
fn text_search_returns_matching_sources_ranked_by_occurrences() {
    let docs = [("a", "rust"), ("b", "Rust rust")];
    assert_eq!(search_text(&docs, "rust"), vec!["b", "a"]);
}

use magi_integrations::{Document, EmbeddingModel, KnowledgeIndex};

#[test]
fn semantic_search_uses_real_cosine_and_checks_model_and_version() {
    let docs = vec![
        Document {
            id: "a".into(),
            version: 1,
            source: "file:a".into(),
            text: "A".into(),
        },
        Document {
            id: "b".into(),
            version: 1,
            source: "file:b".into(),
            text: "B".into(),
        },
    ];
    let mut index = KnowledgeIndex::rebuild(docs).unwrap();
    let model = EmbeddingModel {
        model: "test".into(),
        version: "1".into(),
        dimensions: 2,
    };
    index
        .set_embedding("a", 1, model.clone(), vec![1.0, 0.0])
        .unwrap();
    index
        .set_embedding("b", 1, model.clone(), vec![1.0, 1.0])
        .unwrap();
    let hits = index.semantic_search(&[1.0, 0.0], &model, 0, 10).unwrap();
    assert_eq!(
        hits.iter()
            .map(|h| h.document.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert!(index
        .semantic_search(
            &[1.0, 0.0],
            &EmbeddingModel {
                version: "2".into(),
                ..model
            },
            0,
            10
        )
        .is_err());
}

#[test]
fn checkpoint_restores_vectors_and_model_metadata() {
    let doc = Document {
        id: "a".into(),
        version: 1,
        source: "file:a".into(),
        text: "A".into(),
    };
    let mut index = KnowledgeIndex::rebuild(vec![doc]).unwrap();
    let model = EmbeddingModel {
        model: "model".into(),
        version: "rev".into(),
        dimensions: 2,
    };
    index
        .set_embedding("a", 1, model.clone(), vec![1.0, 0.0])
        .unwrap();
    let restored = KnowledgeIndex::restore(&index.checkpoint().unwrap()).unwrap();
    assert_eq!(
        restored
            .semantic_search(&[1.0, 0.0], &model, 0, 10)
            .unwrap()[0]
            .document
            .id,
        "a"
    );
}

#[test]
fn cancelled_embedding_cannot_publish_late_result() {
    let mut index = KnowledgeIndex::rebuild(vec![Document {
        id: "a".into(),
        version: 1,
        source: "file:a".into(),
        text: "A".into(),
    }])
    .unwrap();
    let ticket = index.begin_embedding("a").unwrap();
    index.cancel_embedding(&ticket);
    let model = EmbeddingModel {
        model: "m".into(),
        version: "1".into(),
        dimensions: 2,
    };
    assert!(index
        .finish_embedding(&ticket, model.clone(), vec![1.0, 0.0])
        .is_err());
    assert!(index.semantic_search(&[1.0, 0.0], &model, 0, 1).is_err());
}

#[test]
fn selected_decision_is_a_versioned_snapshot() {
    let mut store = DecisionStore::default();
    store
        .save(Decision {
            id: "d".into(),
            version: 1,
            text: "first".into(),
        })
        .unwrap();
    let selected = store.select("d", 1).unwrap();
    store
        .save(Decision {
            id: "d".into(),
            version: 2,
            text: "revised".into(),
        })
        .unwrap();
    assert_eq!(selected.text, "first");
    assert_eq!(store.select("d", 1).unwrap(), selected);
    assert!(store
        .save(Decision {
            id: "d".into(),
            version: 1,
            text: "overwrite".into()
        })
        .is_err());
}
