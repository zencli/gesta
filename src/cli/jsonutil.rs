//! The jsonutil commands.

use super::*;

/// Emit an object built outside this module, for a caller that needs the same schema stamp.
pub fn emit_object(fields: Vec<(&str, Json)>) {
    emit(Json::obj(fields));
}

pub(crate) fn emit(v: Json) {
    JSON_PRINTED.store(true, std::sync::atomic::Ordering::Relaxed);
    println!("{}", with_schema(v).render_pretty());
}

/// Wrap a list in an object that names the schema.
///
/// A bare array cannot carry a version, and an agent reading field names needs one. The bare
/// form stays the default because a pipe through `jq '.[0]'` is what people already write, so
/// the envelope is opt in rather than a silent change of shape.
pub fn envelope(key: &str, items: Vec<Json>) -> Json {
    let n = items.len() as i64;
    Json::obj(vec![
        ("schema", Json::str(SCHEMA_VERSION)),
        ("count", Json::int(n)),
        (key, Json::arr(items)),
    ])
}

/// Put `schema` at the top level of an object. Arrays and scalars carry nothing to version.
pub(crate) fn with_schema(v: Json) -> Json {
    match v {
        Json::Obj(mut m) => {
            // A document that already names its schema keeps it: a nested object printing its
            // own version must not be overwritten by the outer one.
            m.entry("schema".to_string())
                .or_insert_with(|| Json::str(SCHEMA_VERSION));
            Json::Obj(m)
        }
        other => other,
    }
}
