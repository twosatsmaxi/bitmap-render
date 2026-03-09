use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TxSummary {
    pub txid: Option<String>,
    pub vsize: u64,
    pub fee: Option<u64>,
    pub feerate: Option<f64>,
    pub value: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockMeta {
    pub id: String,
    pub height: u64,
    pub timestamp: u64,
    pub size: u64,
    pub tx_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Square {
    pub x: i32,
    pub y: i32,
    pub r: i32,
    pub index: usize,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    x: i32,
    y: i32,
    r: i32,
}

#[derive(Clone, Debug)]
struct Row {
    slots: Vec<Slot>,
}

pub struct MondrianLayout {
    pub width: i32,
    rows: Vec<Row>,
}

impl MondrianLayout {
    pub fn new(width: i32) -> Self {
        MondrianLayout {
            width,
            rows: Vec::new(),
        }
    }

    fn ensure_rows(&mut self, y: i32) {
        let target = y as usize + 1;
        if self.rows.len() < target {
            self.rows.resize(target, Row { slots: Vec::new() });
        }
    }

    fn add_slot(&mut self, slot: Slot) {
        if slot.r <= 0 {
            return;
        }
        
        self.ensure_rows(slot.y);
        let row = &mut self.rows[slot.y as usize];
        
        if let Some(existing) = row.slots.iter_mut().find(|s| s.x == slot.x) {
            if slot.r > existing.r {
                existing.r = slot.r;
            }
            return;
        }

        let insert_at = row.slots.iter().position(|s| s.x > slot.x).unwrap_or(row.slots.len());
        row.slots.insert(insert_at, slot);
    }

    fn remove_slot(&mut self, slot_x: i32, slot_y: i32) {
        if let Some(row) = self.rows.get_mut(slot_y as usize) {
            if let Some(pos) = row.slots.iter().position(|s| s.x == slot_x) {
                row.slots.remove(pos);
            }
        }
    }

    fn fill_slot(&mut self, slot: Slot, sw: i32) -> Slot {
        let sq = Slot { x: slot.x, y: slot.y, r: sw };
        self.remove_slot(slot.x, slot.y);

        for ri in slot.y..(slot.y + sw) {
            if (ri as usize) < self.rows.len() {
                let mut max_excess = 0;
                let mut has_next_slot = false;
                
                let row = &mut self.rows[ri as usize];
                
                let mut i = 0;
                while i < row.slots.len() {
                    let ts = row.slots[i];
                    if ts.x == sq.x + sw {
                        has_next_slot = true;
                    }
                    if !((ts.x + ts.r) <= sq.x || ts.x >= (sq.x + sw)) {
                        max_excess = max_excess.max(0.max((ts.x + ts.r) - (slot.x + slot.r)));
                        
                        let modified_r = slot.x - ts.x;
                        if modified_r > 0 {
                            row.slots[i].r = modified_r;
                            i += 1;
                        } else {
                            row.slots.remove(i);
                        }
                    } else {
                        i += 1;
                    }
                }
                
                if sq.x + sw < self.width && !has_next_slot {
                    self.add_slot(Slot { x: sq.x + sw, y: ri, r: slot.r - sw + max_excess });
                }
            } else {
                self.ensure_rows(ri);
                if slot.x > 0 {
                    self.add_slot(Slot { x: 0, y: ri, r: slot.x });
                }
                if sq.x + sw < self.width {
                    self.add_slot(Slot { x: sq.x + sw, y: ri, r: self.width - (sq.x + sw) });
                }
            }
        }

        let min_y = 0.max(slot.y - sw);
        for ri in min_y..slot.y {
            if (ri as usize) >= self.rows.len() {
                continue;
            }
            
            let mut additions = Vec::new();
            let row = &mut self.rows[ri as usize];
            let mut i = 0;
            while i < row.slots.len() {
                let ts = row.slots[i];
                if ts.x < sq.x + sw && ts.x + ts.r > sq.x && ts.y + ts.r >= slot.y {
                    let old_w = ts.r;
                    let new_r = slot.y - ts.y;
                    
                    if new_r <= 0 {
                        row.slots.remove(i);
                    } else {
                        row.slots[i].r = new_r;
                        i += 1;
                    }
                    
                    let mut rem_x = ts.x + ts.r;
                    let mut rem_y = ts.y;
                    let mut rem_w = old_w - new_r;
                    let mut rem_h = new_r;
                    
                    while rem_w > 0 && rem_h > 0 {
                        if rem_w <= rem_h {
                            additions.push(Slot { x: rem_x, y: rem_y, r: rem_w });
                            rem_y += rem_w;
                            rem_h -= rem_w;
                        } else {
                            additions.push(Slot { x: rem_x, y: rem_y, r: rem_h });
                            rem_x += rem_h;
                            rem_w -= rem_h;
                        }
                    }
                } else {
                    i += 1;
                }
            }
            
            for add in additions {
                self.add_slot(add);
            }
        }

        sq
    }

    pub fn place(&mut self, size: i32) -> (i32, i32, i32) {
        let mut found_sq = None;
        for row_idx in 0..self.rows.len() {
            let mut found_slot = None;
            for slot in &self.rows[row_idx].slots {
                if slot.r >= size {
                    found_slot = Some(*slot);
                    break;
                }
            }
            if let Some(slot) = found_slot {
                found_sq = Some(self.fill_slot(slot, size));
                break;
            }
        }

        let sq = if let Some(sq) = found_sq {
            sq
        } else {
            let new_row_y = self.rows.len() as i32;
            self.ensure_rows(new_row_y);
            let slot = Slot { x: 0, y: new_row_y, r: self.width };
            self.add_slot(slot);
            self.fill_slot(slot, size)
        };
        (sq.x, sq.y, sq.r)
    }
}

pub fn compute_layout(sizes: &[u8]) -> (i32, i32, Vec<Square>) {
    let mut weight = 0i64;
    for &size in sizes {
        let size = if size == 0 { 1 } else { size as i32 };
        weight += (size as i64) * (size as i64);
    }
    let width = (weight as f64).sqrt().ceil() as i32;
    
    let mut layout = MondrianLayout::new(width);
    let mut squares = Vec::with_capacity(sizes.len());
    let mut max_y = 0;
    
    for (i, &size) in sizes.iter().enumerate() {
        let size = if size == 0 { 1 } else { size as i32 };
        let (x, y, r) = layout.place(size);
        squares.push(Square { x, y, r, index: i });
        max_y = max_y.max(y + r);
    }
    
    (width, max_y, squares)
}
