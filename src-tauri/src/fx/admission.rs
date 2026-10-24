//! Bounded static death -> emission chain, proved with complete real vehicles.
//! This only classifies the graph; all ordinary compile/render guards still run.
use super::*;

fn has_custom_frame(source: &Value) -> bool {
    [
        "previewEmitterFrame",
        "previewEmitterFrames",
        "previewEmitterKeyframes",
        "previewSourceFrames",
    ]
    .iter()
    .any(|key| source.get(*key).is_some())
}

fn has_edges(layer: &Definition) -> bool {
    !layer.children.is_empty() || layer.death.is_some() || layer.emission.is_some()
}

fn compile_bounded(source: &Value, maximum: usize) -> Option<Vec<Definition>> {
    if has_custom_frame(source) {
        return None;
    }
    let layers = compile_effect(source, true).ok()?;
    if layers.is_empty() || layers.len() > maximum {
        return None;
    }
    if layers.iter().any(|layer| {
        !layer.children.is_empty()
            || layer.sound
            || layer.impact.is_some()
            || layer.runtime.contains("useCollision")
    }) {
        return None;
    }
    Some(layers)
}

pub(super) fn accepts(effect: &Value) -> bool {
    let Some(registry) = effect["dependencies"].as_object() else {
        return false;
    };
    if registry.len() != 2 {
        return false;
    }
    let root_name = canonical(effect["name"].as_str().unwrap_or(""));
    if root_name.is_empty() || registry.contains_key(&root_name) {
        return false;
    }
    let Some(root) = compile_bounded(effect, 16) else {
        return false;
    };
    if root.iter().any(|layer| {
        !["billboardSprite", "tail"].contains(&layer.kind.as_str()) || layer.emission.is_some()
    }) {
        return false;
    }
    let deaths = root
        .iter()
        .filter_map(|layer| layer.death.as_ref())
        .collect::<Vec<_>>();
    if deaths.len() != 1 {
        return false;
    }
    let middle_name = canonical(deaths[0]);
    let Some(middle_source) = registry.get(&middle_name) else {
        return false;
    };
    if middle_name == root_name
        || canonical(middle_source["name"].as_str().unwrap_or("")) != middle_name
    {
        return false;
    }
    let Some(middle) = compile_bounded(middle_source, 20) else {
        return false;
    };
    if middle.iter().any(|layer| {
        !["billboardSprite", "line", "cloud", "light"].contains(&layer.kind.as_str())
            || layer.death.is_some()
    }) || middle.iter().filter(|layer| layer.kind == "cloud").count() != 1
        || middle.iter().filter(|layer| layer.kind == "light").count() != 1
    {
        return false;
    }
    let emissions = middle
        .iter()
        .filter_map(|layer| layer.emission.as_ref())
        .collect::<Vec<_>>();
    if emissions.len() != 1 {
        return false;
    }
    let leaf_name = canonical(emissions[0]);
    let Some(leaf_source) = registry.get(&leaf_name) else {
        return false;
    };
    if leaf_name == root_name
        || leaf_name == middle_name
        || canonical(leaf_source["name"].as_str().unwrap_or("")) != leaf_name
    {
        return false;
    }
    let Some(leaf) = compile_bounded(leaf_source, 2) else {
        return false;
    };
    if leaf
        .iter()
        .any(|layer| layer.kind != "billboardSprite" || has_edges(layer))
    {
        return false;
    }
    root.len() + middle.len() + leaf.len() <= 38
}
