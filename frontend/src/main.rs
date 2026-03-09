use yew::prelude::*;
use common::{compute_layout, BlockMeta, Square};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};
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

        use_effect_with((), move |_| {
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

fn render_squares(ctx: &CanvasRenderingContext2d, squares: &[Square], layout_width: i32, used_h: i32, canvas_size: i32) {
    ctx.set_fill_style(&"#0d1117".into());
    ctx.fill_rect(0.0, 0.0, canvas_size as f64, canvas_size as f64);
    
    let draw = layout_width.max(used_h) as f64;
    let grid_size = canvas_size as f64 / draw;
    let offset_y = (canvas_size as f64 - used_h as f64 * grid_size) / 2.0;
    let unit_padding = grid_size / 4.0;
    
    ctx.set_fill_style(&"#f7a23b".into()); // Simplified orange
    
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

    html! {
        <main>
            <div id="toolbar">
                <div id="title">{ "Lite Block Grid (Yew)" }</div>
            </div>
            <div id="grid">
                {
                    for (0..4).map(|i| {
                        html! { <BlockCard height={*start_height + i} api_base={api_base.clone()} /> }
                    })
                }
            </div>
        </main>
    }
}

fn main() {
    yew::Renderer::<App>::new().render();
}
