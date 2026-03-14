use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn layout_block(sizes: &[u8]) -> js_sys::Int32Array {
    let (width, used_height, squares) = common::compute_layout(sizes);

    // Flat layout: [width, usedHeight, x0, y0, r0, x1, y1, r1, ...]
    let mut out = Vec::with_capacity(2 + squares.len() * 3);
    out.push(width);
    out.push(used_height);
    for sq in &squares {
        out.push(sq.x);
        out.push(sq.y);
        out.push(sq.r);
    }

    js_sys::Int32Array::from(&out[..])
}
