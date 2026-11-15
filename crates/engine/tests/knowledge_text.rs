#![cfg(windows)]
use domain::{Command, Request, ResponseData};
use engine::Engine;
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("magi-knowledge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        Self {
            db: root.join("db.redb"),
            root,
        }
    }
    fn engine(&self) -> Arc<Engine> {
        Arc::new(Engine::open(&self.db).unwrap())
    }
    fn project(&self, engine: &Engine, name: &str) -> String {
        match engine
            .handle(Request {
                id: "create".into(),
                command: Command::CreateProject {
                    name: name.into(),
                    path: self.root.to_string_lossy().into_owned(),
                },
            })
            .result
            .unwrap()
        {
            ResponseData::Project(p) => p.id,
            _ => panic!("project"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db);
        let _ = std::fs::remove_dir(&self.root);
    }
}
async fn send(engine: &Arc<Engine>, command: Command) -> Result<ResponseData, String> {
    engine
        .clone()
        .handle_async(Request {
            id: "test".into(),
            command,
        })
        .await
        .result
}
fn data(response: ResponseData) -> serde_json::Value {
    match response {
        ResponseData::Data(v) => v,
        _ => panic!("data"),
    }
}
fn doc(id: &str, version: u64, source: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"id":id,"version":version,"source":source,"text":text})
}

#[tokio::test]
async fn text_documents_survive_restart_and_remain_project_scoped() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let a = fixture.project(&engine, "a");
    let b = fixture.project(&engine, "b");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: a.clone(),
            document: doc("one", 1, "original-file", "hello HELLO world"),
        },
    )
    .await
    .unwrap();
    send(
        &engine,
        Command::PutKnowledge {
            project_id: b.clone(),
            document: doc("two", 1, "other-file", "hello"),
        },
    )
    .await
    .unwrap();
    drop(engine);
    let engine = fixture.engine();
    let source = data(
        send(
            &engine,
            Command::GetKnowledgeSource {
                project_id: a.clone(),
                document_id: "one".into(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(source["source"], "original-file");
    assert_eq!(source["text"], "hello HELLO world");
    let search = data(
        send(
            &engine,
            Command::SearchKnowledgePage {
                project_id: a.clone(),
                query: "hello".into(),
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(search["mode"], "text");
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
    assert_eq!(search["results"][0]["id"], "one");
    assert!(send(
        &engine,
        Command::GetKnowledgeSource {
            project_id: b,
            document_id: "one".into()
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::PutKnowledge {
            project_id: a.clone(),
            document: doc("one", 1, "changed", "not saved")
        }
    )
    .await
    .is_err());
    let source = data(
        send(
            &engine,
            Command::GetKnowledgeSource {
                project_id: a,
                document_id: "one".into(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(source["source"], "original-file");
}

#[tokio::test]
async fn text_search_validates_inputs_and_pages_in_stable_order() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    for (name, text) in [("b", "needle"), ("a", "needle"), ("c", "needle needle")] {
        send(
            &engine,
            Command::PutKnowledge {
                project_id: id.clone(),
                document: doc(name, 1, "fixture", text),
            },
        )
        .await
        .unwrap();
    }
    let page = data(
        send(
            &engine,
            Command::SearchKnowledgePage {
                project_id: id.clone(),
                query: "needle".into(),
                offset: 1,
                limit: 1,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(page["results"].as_array().unwrap().len(), 1);
    assert_eq!(page["results"][0]["id"], "a");
    let search = data(
        send(
            &engine,
            Command::SearchKnowledge {
                project_id: id.clone(),
                query: "needle".into(),
                semantic: false,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(search["results"][0]["id"], "c");
    assert!(send(
        &engine,
        Command::SearchKnowledgePage {
            project_id: id.clone(),
            query: "  ".into(),
            offset: 0,
            limit: 1
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::SearchKnowledge {
            project_id: id.clone(),
            query: "needle".into(),
            semantic: true
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("", 1, "fixture", "text")
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("x", 0, "fixture", "text")
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::PutKnowledge {
            project_id: "missing".into(),
            document: doc("x", 1, "fixture", "text")
        }
    )
    .await
    .is_err());
}

#[tokio::test]
async fn damaged_checkpoint_is_rebuilt_from_durable_original() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("one", 1, "original", "needle"),
        },
    )
    .await
    .unwrap();
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("integration_knowledge"))
            .unwrap();
        table.insert(id.as_str(), "broken").unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let source = data(
        send(
            &engine,
            Command::GetKnowledgeSource {
                project_id: id.clone(),
                document_id: "one".into(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(source["source"], "original");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("one", 2, "revised", "needle again"),
        },
    )
    .await
    .unwrap();
    drop(engine);
    let engine = fixture.engine();
    let source = data(
        send(
            &engine,
            Command::GetKnowledgeSource {
                project_id: id,
                document_id: "one".into(),
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(source["source"], "revised");
    assert_eq!(source["version"], 2);
}

#[tokio::test]
async fn text_search_caps_pages_at_one_hundred_documents() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    drop(engine);
    let docs: Vec<integrations::Document> = (0..105)
        .map(|i| integrations::Document {
            id: format!("doc-{i:03}"),
            version: 1,
            source: "fixture".into(),
            text: "needle".into(),
        })
        .collect();
    let checkpoint = integrations::KnowledgeIndex::rebuild(docs.clone())
        .unwrap()
        .checkpoint()
        .unwrap();
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new(
                "integration_knowledge_sources",
            ))
            .unwrap();
        table
            .insert(id.as_str(), serde_json::to_string(&docs).unwrap().as_str())
            .unwrap();
    }
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("integration_knowledge"))
            .unwrap();
        table
            .insert(id.as_str(), std::str::from_utf8(&checkpoint).unwrap())
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let page = data(
        send(
            &engine,
            Command::SearchKnowledgePage {
                project_id: id.clone(),
                query: "needle".into(),
                offset: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(page["results"].as_array().unwrap().len(), 100);
    let tail = data(
        send(
            &engine,
            Command::SearchKnowledgePage {
                project_id: id,
                query: "needle".into(),
                offset: 100,
                limit: 1000,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(tail["results"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn vectors_survive_restart_and_are_isolated_by_project_and_model() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let a = fixture.project(&engine, "a");
    let b = fixture.project(&engine, "b");
    for (id, text) in [("near", "near original"), ("far", "far original")] {
        send(
            &engine,
            Command::PutKnowledge {
                project_id: a.clone(),
                document: doc(id, 1, id, text),
            },
        )
        .await
        .unwrap();
    }
    let model = serde_json::json!({"model":"fixture","version":"v1","dimensions":2});
    for (id, vector) in [("near", vec![1.0, 0.0]), ("far", vec![0.0, 1.0])] {
        send(
            &engine,
            Command::SetKnowledgeEmbedding {
                project_id: a.clone(),
                document_id: id.into(),
                version: 1,
                model: model.clone(),
                vector,
            },
        )
        .await
        .unwrap();
    }
    drop(engine);
    let engine = fixture.engine();
    let result = data(
        send(
            &engine,
            Command::SearchKnowledgeVector {
                project_id: a.clone(),
                model: model.clone(),
                vector: vec![1.0, 0.0],
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(result["mode"], "semantic");
    assert_eq!(result["results"][0]["document"]["id"], "near");
    assert_eq!(result["results"][0]["document"]["source"], "near");
    assert!(send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: b,
            model: model.clone(),
            vector: vec![1.0, 0.0],
            offset: 0,
            limit: 10
        }
    )
    .await
    .is_err());
    assert!(send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: a,
            model: serde_json::json!({"model":"fixture","version":"other","dimensions":2}),
            vector: vec![1.0, 0.0],
            offset: 0,
            limit: 10
        }
    )
    .await
    .is_err());
}

#[tokio::test]
async fn vector_validation_rejects_invalid_input_and_document_updates_invalidate_vectors() {
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("one", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    let model = serde_json::json!({"model":"fixture","version":"v1","dimensions":2});
    for (version, candidate_model, vector) in [
        (2, model.clone(), vec![1.0, 0.0]),
        (1, model.clone(), vec![1.0]),
        (1, model.clone(), vec![0.0, 0.0]),
        (1, model.clone(), vec![f32::NAN, 1.0]),
        (
            1,
            serde_json::json!({"model":"","version":"v1","dimensions":2}),
            vec![1.0, 0.0],
        ),
    ] {
        assert!(send(
            &engine,
            Command::SetKnowledgeEmbedding {
                project_id: id.clone(),
                document_id: "one".into(),
                version,
                model: candidate_model,
                vector
            }
        )
        .await
        .is_err());
    }
    send(
        &engine,
        Command::SetKnowledgeEmbedding {
            project_id: id.clone(),
            document_id: "one".into(),
            version: 1,
            model: model.clone(),
            vector: vec![1.0, 0.0],
        },
    )
    .await
    .unwrap();
    assert!(send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: id.clone(),
            model: model.clone(),
            vector: vec![f32::INFINITY, 0.0],
            offset: 0,
            limit: 10
        }
    )
    .await
    .is_err());
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("one", 2, "updated", "new text"),
        },
    )
    .await
    .unwrap();
    assert!(send(
        &engine,
        Command::SearchKnowledgeVector {
            project_id: id.clone(),
            model: model.clone(),
            vector: vec![1.0, 0.0],
            offset: 0,
            limit: 10
        }
    )
    .await
    .is_err());
    let text = data(
        send(
            &engine,
            Command::SearchKnowledgePage {
                project_id: id,
                query: "new".into(),
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap(),
    );
    assert_eq!(text["results"][0]["source"], "updated");
}

#[tokio::test]
async fn damaged_vector_checkpoint_is_not_overwritten_by_embedding_update() {
    use redb::{Database, TableDefinition};
    let fixture = Fixture::new();
    let engine = fixture.engine();
    let id = fixture.project(&engine, "project");
    send(
        &engine,
        Command::PutKnowledge {
            project_id: id.clone(),
            document: doc("one", 1, "source", "text"),
        },
    )
    .await
    .unwrap();
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx
            .open_table(TableDefinition::<&str, &str>::new("integration_knowledge"))
            .unwrap();
        table.insert(id.as_str(), "damaged").unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    let engine = fixture.engine();
    let result = send(
        &engine,
        Command::SetKnowledgeEmbedding {
            project_id: id.clone(),
            document_id: "one".into(),
            version: 1,
            model: serde_json::json!({"model":"fixture","version":"v1","dimensions":2}),
            vector: vec![1.0, 0.0],
        },
    )
    .await;
    assert!(result.is_err());
    drop(engine);
    let db = Database::open(&fixture.db).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx
        .open_table(TableDefinition::<&str, &str>::new("integration_knowledge"))
        .unwrap();
    assert_eq!(table.get(id.as_str()).unwrap().unwrap().value(), "damaged");
}
