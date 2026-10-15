//! Public execution contracts across dependency upgrades.

use octofhir_sof::{ViewDefinition, execute, execute_parallel};
use serde_json::json;

#[tokio::test]
async fn parallel_execution_preserves_rows_indices_and_contained_keys() {
    let view = ViewDefinition::from_json(&json!({
        "resource": "Patient",
        "select": [
            {"column": [
                {"name": "id", "path": "id"},
                {"name": "ref", "path": "managingOrganization.getReferenceKey(Organization)"},
                {"name": "key", "path": "contained.getResourceKey()"}
            ]},
            {"forEach": "name", "column": [
                {"name": "index", "path": "%rowIndex", "type": "integer"},
                {"name": "family", "path": "family"}
            ]}
        ]
    }))
    .unwrap();
    let resources: Vec<_> = (0..4)
        .map(|i| {
            json!({
                "resourceType": "Patient", "id": format!("p{i}"),
                "contained": [{"resourceType": "Organization", "id": "org"}],
                "managingOrganization": {"reference": "#org"},
                "name": [{"family": "First"}, {"family": "Second"}]
            })
        })
        .collect();
    let expected: Vec<_> = (0..4)
        .flat_map(|i| {
            [
                vec![
                    json!(format!("p{i}")),
                    json!("org"),
                    json!("org"),
                    json!(0),
                    json!("First"),
                ],
                vec![
                    json!(format!("p{i}")),
                    json!("org"),
                    json!("org"),
                    json!(1),
                    json!("Second"),
                ],
            ]
        })
        .collect();
    let serial = execute(&view, &resources).await.unwrap();
    assert_eq!(serial.data, expected);
    for threads in [0, 1, 2, 8] {
        let parallel = execute_parallel(&view, &resources, threads).unwrap();
        assert_eq!(parallel.data, expected, "threads={threads}");
        assert_eq!(parallel.row_count, expected.len());
        assert_eq!(parallel.columns.len(), serial.columns.len());
    }
}
