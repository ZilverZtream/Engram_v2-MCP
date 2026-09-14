#![allow(clippy::unwrap_used)]

use engram_core::Config;
use engram_server::{models::FindSymbolReferencesRequest, state::AppState, tools::Engram};
use serde_json::json;

fn configure_sidecar() -> bool {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tools/vb_roslyn_sidecar/publish_out")
        .join(if cfg!(windows) {
            "vb_roslyn_sidecar.exe"
        } else {
            "vb_roslyn_sidecar"
        });
    let path = if cfg!(windows) {
        path.parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("bin/Release/net8.0/vb_roslyn_sidecar.exe")
    } else {
        path
    };
    if !path.exists() {
        return false;
    }
    unsafe { std::env::set_var("ENGRAM_VB_SIDECAR_PATH", path) };
    true
}

async fn references(engram: &Engram, project_id: &str, symbol: &str) -> String {
    let request: FindSymbolReferencesRequest = serde_json::from_value(json!({
        "project_id": project_id,
        "symbol_name": symbol,
    }))
    .unwrap();
    engram
        .handle_find_symbol_references(request)
        .await
        .unwrap()
        .content[0]
        .as_text()
        .unwrap()
        .text
        .clone()
}

#[tokio::test]
async fn vb_property_getters_connect_consumers_and_accessor_dependencies() {
    if !configure_sidecar() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Rules.vb"),
        "Namespace Demo\n\
         Public Class Worker\n\
          Public Shared Function Check() As Boolean\n\
           Return True\n\
          End Function\n\
         End Class\n\
         Public Class Rules\n\
          Public Shared ReadOnly Property Allowed As Boolean\n\
           Get\n\
            Return Worker.Check()\n\
           End Get\n\
          End Property\n\
          Public Shared Property AutoFlag As Boolean\n\
         End Class\n\
        End Namespace\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Consumer.vb"),
        "Namespace Demo\n\
         Public Class Consumer\n\
          Public Sub Run()\n\
           Dim state = New With {.Flag = False}\n\
           With state\n\
            .Flag = True\n\
           End With\n\
           If Rules.Allowed Then Return\n\
           Rules.AutoFlag = True\n\
          End Sub\n\
         End Class\n\
        End Namespace\n",
    )
    .unwrap();

    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram
        .handle_index_project(
            serde_json::from_value(json!({
                "directory": root,
                "project_name": "vb-property-graph",
                "project_type": "dotnet_webforms_vb",
                "wait": true,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let project_id = &state.registry.list_projects().unwrap()[0].project_id;

    let property = references(&engram, project_id, "Demo.Rules.Allowed").await;
    assert!(property.contains("Demo.Consumer.Run"), "{property}");
    assert!(property.contains("indexed call sites L8"), "{property}");

    let auto_property = references(&engram, project_id, "Demo.Rules.AutoFlag").await;
    assert!(
        auto_property.contains("Demo.Consumer.Run"),
        "{auto_property}"
    );
    assert!(
        auto_property.contains("indexed call sites L9"),
        "{auto_property}"
    );
    assert!(
        auto_property.contains("via property_set"),
        "{auto_property}"
    );

    let downstream = references(&engram, project_id, "Demo.Worker.Check").await;
    assert!(downstream.contains("Demo.Rules.Allowed"), "{downstream}");
    assert!(
        downstream.contains("indexed call sites L10"),
        "{downstream}"
    );
}
