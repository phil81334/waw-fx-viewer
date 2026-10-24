//! Bounded model-only ordering recovered from the native phase/key probes.
//! Model surfaces stay before emissive particles. Equal-primary *different*
//! materials need their original verified ranks; names are never a rank.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub(super) struct ModelOrderInput {
    pub index: usize,
    pub material: String,
    pub primary: u32,
    pub verified_material_rank: Option<u32>,
}

pub(super) fn ordered_indices(rows: &[ModelOrderInput]) -> Result<Vec<usize>, String> {
    let mut peers: BTreeMap<u32, BTreeSet<&str>> = BTreeMap::new();
    for row in rows {
        if row.primary >= 64 {
            return Err("Model primary sort key exceeds native six bits".into());
        }
        peers.entry(row.primary).or_default().insert(&row.material);
    }
    for row in rows {
        if peers[&row.primary].len() > 1 && row.verified_material_rank.is_none() {
            return Err(format!(
                "Unproved equal-primary model material rank: {} (primary {})",
                row.material, row.primary
            ));
        }
    }
    let mut ordered: Vec<&ModelOrderInput> = rows.iter().collect();
    ordered.sort_by_key(|row| {
        (
            // Original unlit class0 (<24) goes to bucket2, class1 to bucket5.
            // Sorting model primary keys preserves the phase split automatically.
            row.primary,
            row.verified_material_rank.unwrap_or(0),
            // Input index must preserve original scene admission / surface order.
            // This is not a substitute for an arbitrary JSON geometry permutation.
            row.index,
        )
    });
    Ok(ordered.into_iter().map(|row| row.index).collect())
}
