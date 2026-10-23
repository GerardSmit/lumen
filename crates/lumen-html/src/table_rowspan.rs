//! Distribution of rowspan cell heights over the rows they span.
use alloc::vec::Vec;

#[derive(Clone, Copy)]
pub(crate) struct RowspanRequest {
    pub start: usize,
    pub end: usize,
    pub required: f32,
}

const SELECTION_LIMIT: usize = 64;

fn encloses(outer: &RowspanRequest, inner: &RowspanRequest) -> bool {
    outer.start <= inner.start && outer.end >= inner.end
        && (outer.start != inner.start || outer.end != inner.end)
}

/// Orders requests so that a cell enclosed by another goes first, cells over
/// the same rows go tallest first, and otherwise the earlier row goes first.
fn order(mut requests: Vec<RowspanRequest>) -> Vec<RowspanRequest> {
    let key = |a: &RowspanRequest, b: &RowspanRequest| {
        a.start.cmp(&b.start).then(b.required.total_cmp(&a.required))
    };
    if requests.len() > SELECTION_LIMIT {
        requests.sort_by(key);
        return requests;
    }
    let mut ordered = Vec::with_capacity(requests.len());
    while !requests.is_empty() {
        let mut best: Option<usize> = None;
        for (index, candidate) in requests.iter().enumerate() {
            if requests.iter().any(|other| encloses(candidate, other)) { continue; }
            let better = match best {
                None => true,
                Some(current) => key(candidate, &requests[current]).is_lt(),
            };
            if better { best = Some(index); }
        }
        ordered.push(requests.remove(best.unwrap_or(0)));
    }
    ordered
}

/// Grows rows so each rowspan request fits. Unconstrained non-empty rows grow
/// in proportion to their height; failing that, constrained non-empty rows do;
/// a span of only empty rows grows its last row.
pub(crate) fn distribute_rowspans(heights: &mut [f32], constrained: &[bool], requests: Vec<RowspanRequest>, spacing: f32) {
    for request in order(requests) {
        let end = request.end.min(heights.len());
        if request.start >= end { continue; }
        let rows = request.start..end;
        let present = heights[rows.clone()].iter().sum::<f32>() + spacing * (rows.len() - 1) as f32;
        let extra = request.required - present;
        if extra <= 0.0 { continue; }
        for want_constrained in [false, true] {
            let total: f32 = rows.clone()
                .filter(|row| constrained.get(*row).copied().unwrap_or(false) == want_constrained && heights[*row] > 0.0)
                .map(|row| heights[row]).sum();
            if total <= 0.0 { continue; }
            for row in rows.clone() {
                if constrained.get(row).copied().unwrap_or(false) == want_constrained && heights[row] > 0.0 {
                    heights[row] += extra * heights[row] / total;
                }
            }
            break;
        }
        let grown = heights[rows.clone()].iter().sum::<f32>() + spacing * (rows.len() - 1) as f32;
        if grown < request.required - 0.001 {
            heights[end - 1] += request.required - grown;
        }
    }
}

/// Grows the rows of a row group to its specified height. Unconstrained
/// non-empty rows grow in proportion to their height, then unconstrained empty
/// rows evenly, then constrained non-empty rows in proportion.
pub(crate) fn distribute_group_height(heights: &mut [f32], constrained: &[bool], rows: core::ops::Range<usize>, required: f32, spacing: f32) {
    let end = rows.end.min(heights.len());
    if rows.start >= end { return; }
    let rows = rows.start..end;
    let extra = required - (heights[rows.clone()].iter().sum::<f32>() + spacing * (rows.len() - 1) as f32);
    if extra <= 0.0 { return; }
    let is_constrained = |row: usize| constrained.get(row).copied().unwrap_or(false);
    let total: f32 = rows.clone().filter(|row| !is_constrained(*row) && heights[*row] > 0.0).map(|row| heights[row]).sum();
    if total > 0.0 {
        for row in rows.clone() {
            if !is_constrained(row) && heights[row] > 0.0 { heights[row] += extra * heights[row] / total; }
        }
        return;
    }
    let empty: Vec<usize> = rows.clone().filter(|row| !is_constrained(*row) && heights[*row] <= 0.0).collect();
    if !empty.is_empty() {
        let share = extra / empty.len() as f32;
        for row in empty { heights[row] += share; }
        return;
    }
    let total: f32 = rows.clone().map(|row| heights[row]).sum();
    if total > 0.0 {
        for row in rows { heights[row] += extra * heights[row] / total; }
    } else {
        let share = extra / rows.len() as f32;
        for row in rows { heights[row] += share; }
    }
}
