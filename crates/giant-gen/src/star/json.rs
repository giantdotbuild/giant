//! Convert parsed data (`serde_json::Value`, which YAML also decodes into) into
//! Starlark values, so `parse_json`/`parse_yaml`/`parse_json_stream` hand the
//! script ordinary dicts, lists, and scalars.

use anyhow::{Result, bail};
use serde_json::Value as Json;
use starlark::values::dict::AllocDict;
use starlark::values::{Heap, Value};

pub(crate) fn to_value<'v>(json: &Json, heap: Heap<'v>) -> Result<Value<'v>> {
    Ok(match json {
        Json::Null => Value::new_none(),
        Json::Bool(b) => Value::new_bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                heap.alloc(i)
            } else if let Some(f) = n.as_f64() {
                heap.alloc(f)
            } else {
                bail!("unrepresentable JSON number: {n}")
            }
        }
        Json::String(s) => heap.alloc(s.as_str()),
        Json::Array(items) => {
            let elems = items
                .iter()
                .map(|i| to_value(i, heap))
                .collect::<Result<Vec<_>>>()?;
            heap.alloc(elems)
        }
        Json::Object(map) => {
            let entries = map
                .iter()
                .map(|(k, v)| Ok((heap.alloc(k.as_str()), to_value(v, heap)?)))
                .collect::<Result<Vec<_>>>()?;
            heap.alloc(AllocDict(entries))
        }
    })
}
