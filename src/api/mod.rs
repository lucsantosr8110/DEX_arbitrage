use axum::{
    extract::Extension,
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;
use std::path::PathBuf;
use std::{fs, sync::Arc};

pub fn create_routes(config_path: Arc<PathBuf>) -> Router {
    Router::new()
        .route("/config", get(get_config))
        .route("/config", post(update_config))
        .layer(Extension(config_path))
}

async fn get_config(Extension(config_path): Extension<Arc<PathBuf>>) -> Json<Value> {
    let content = fs::read_to_string(&*config_path).unwrap_or_default();
    let parsed =
        toml::from_str::<toml::Value>(&content).unwrap_or(toml::Value::Table(Default::default()));
    Json(serde_json::to_value(parsed).unwrap())
}

async fn update_config(
    Extension(config_path): Extension<Arc<PathBuf>>,
    Json(body): Json<Value>,
) -> Json<&'static str> {
    let toml_str = toml::to_string(&body).expect("Erro ao gerar TOML");
    fs::write(&*config_path, toml_str).expect("Erro ao salvar config");
    Json("Config atualizada com sucesso")
}
