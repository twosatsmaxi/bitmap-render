use yew::prelude::*;
use common::{compute_layout, BlockMeta, Square};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlInputElement};
use wasm_bindgen::JsCast;
use gloo_net::http::Request;

#[derive(Properties, PartialEq, Clone)]
pub struct BlockCardProps {
    pub height: u64,
    pub api_base: String,
}

#[function_component(BlockCard)]
fn block_card(props: &BlockCardProps) -> Html {
    let canvas_ref = use_node_ref();
    let meta = use_state(|| None::<BlockMeta>);
    let error = use_state(|| None::<String>);

    {
        let height = props.height;
        let api_base = props.api_base.clone();
        let meta = meta.clone();
        let error = error.clone();
        let canvas_ref = canvas_ref.clone();

        use_effect_with(height, move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                // Fetch Meta
                let meta_url = format!("{}/api/block/{}/meta", api_base, height);
                let resp = Request::get(&meta_url).send().await;
                
                match resp {
                    Ok(r) if r.ok() => {
                        if let Ok(m) = r.json::<BlockMeta>().await {
                            meta.set(Some(m));
                        }
                    }
                    _ => error.set(Some("Failed to load meta".into())),
                }

                // Fetch Binary Data
                let data_url = format!("{}/api/block/{}", api_base, height);
                let resp = Request::get(&data_url).send().await;

                if let Ok(r) = resp {
                    if r.ok() {
                        if let Ok(buffer) = r.binary().await {
                            let (width, used_height, squares) = compute_layout(&buffer);
                            
                            if let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() {
                                let context = canvas
                                    .get_context("2d")
                                    .unwrap()
                                    .unwrap()
                                    .dyn_into::<CanvasRenderingContext2d>()
                                    .unwrap();

                                render_squares(&context, &squares, width, used_height, 240);
                            }
                        }
                    }
                }
            });
            || ()
        });
    }

    html! {
        <div class="card">
            <div class="card-head">
                <div class="height">{ format!("#{}", props.height) }</div>
                <div class="meta">
                    {
                        if let Some(m) = &*meta {
                            format!("{} tx", m.tx_count)
                        } else if let Some(e) = &*error {
                            e.clone()
                        } else {
                            "loading".into()
                        }
                    }
                </div>
            </div>
            <div class="canvas-wrap">
                <canvas ref={canvas_ref} width="240" height="240"></canvas>
            </div>
        </div>
    }
}

fn hcl_to_rgb(h_deg: f64, c: f64, l: f64) -> (u8, u8, u8) {
    let h = h_deg * std::f64::consts::PI / 180.0;
    let a = h.cos() * c;
    let b = h.sin() * c;
    let fy = (l + 16.0) / 116.0;
    let fx = a / 500.0 + fy;
    let fz = fy - b / 200.0;
    let e = 0.008856;
    let k = 903.3;
    let x = if fx.powi(3) > e { fx.powi(3) } else { (116.0 * fx - 16.0) / k } * 0.95047;
    let y = if l > k * e { ((l + 16.0) / 116.0).powi(3) } else { l / k };
    let z = if fz.powi(3) > e { fz.powi(3) } else { (116.0 * fz - 16.0) / k } * 1.08883;
    let lin = |v: f64| if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (
        (lin(x * 3.2406 + y * -1.5372 + z * -0.4986) * 255.0).round().clamp(0.0, 255.0) as u8,
        (lin(x * -0.9689 + y * 1.8758 + z * 0.0415) * 255.0).round().clamp(0.0, 255.0) as u8,
        (lin(x * 0.0557 + y * -0.2040 + z * 1.0570) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn render_squares(ctx: &CanvasRenderingContext2d, squares: &[Square], layout_width: i32, used_h: i32, canvas_size: i32) {
    ctx.set_fill_style(&"#0d1117".into());
    ctx.fill_rect(0.0, 0.0, canvas_size as f64, canvas_size as f64);
    
    let draw = layout_width.max(used_h) as f64;
    let grid_size = canvas_size as f64 / draw;
    let offset_y = (canvas_size as f64 - used_h as f64 * grid_size) / 2.0;
    let unit_padding = grid_size / 4.0;
    
    let (r, g, b) = hcl_to_rgb(0.181 * 360.0, 78.225, 0.472 * 150.0);
    ctx.set_fill_style(&format!("rgb({},{},{})", r, g, b).into());
    
    for sq in squares {
        let px = sq.x as f64 * grid_size + unit_padding;
        let py = sq.y as f64 * grid_size + offset_y + unit_padding;
        let pw = sq.r as f64 * grid_size - unit_padding * 2.0;
        if pw <= 0.0 { continue; }
        ctx.fill_rect(px, py, pw, pw);
    }
}

#[function_component(App)]
fn app() -> Html {
    let start_height = use_state(|| 840000u64);
    let api_base = "http://localhost:3000".to_string();
    let search_input_ref = use_node_ref();

    let on_prev = {
        let start_height = start_height.clone();
        Callback::from(move |_| {
            start_height.set(start_height.saturating_sub(4));
        })
    };

    let on_next = {
        let start_height = start_height.clone();
        Callback::from(move |_| {
            start_height.set(*start_height + 4);
        })
    };

    let on_search = {
        let start_height = start_height.clone();
        let search_input_ref = search_input_ref.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if let Some(input) = search_input_ref.cast::<HtmlInputElement>() {
                if let Ok(h) = input.value().parse::<u64>() {
                    start_height.set(h);
                }
            }
        })
    };

    html! {
        <main>
            <div id="toolbar">
                <div id="title">{ "Lite Block Grid (Yew)" }</div>
                <button class="step-btn" onclick={on_prev}>{ "-4" }</button>
                <button class="step-btn" onclick={on_next}>{ "+4" }</button>
                <form id="search-form" onsubmit={on_search}>
                    <input 
                        ref={search_input_ref}
                        id="search-input" 
                        type="text" 
                        placeholder="start block height" 
                        autocomplete="off" 
                        spellcheck="false" 
                    />
                    <button id="search-btn" type="submit">{ "Load" }</button>
                </form>
            </div>
            <div id="grid">
                {
                    for (0..4).map(|i| {
                        html! { <BlockCard key={*start_height + i} height={*start_height + i} api_base={api_base.clone()} /> }
                    })
                }
            </div>
        </main>
    }
}

fn main() {
    yew::Renderer::<App>::new().render();
}
