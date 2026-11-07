//! 布局算法：浮动（级联放置）与网格排列。
//!
//! 全部为纯函数/纯状态，坐标为全局逻辑像素，供 agentdesk 的窗口管理调用。

/// 在输出区域内为新窗口挑选级联偏移位置：
/// 依次尝试错开的左上角，避免完全覆盖既有窗口。
#[derive(Debug, Default)]
pub struct FloatingLayout {
    cascade_step: i32,
}

impl FloatingLayout {
    pub fn new() -> Self {
        Self { cascade_step: 0 }
    }

    /// 返回新窗口的左上角坐标。
    ///
    /// `occupied` 为既有窗口几何（x, y, w, h）；策略为从 (32, 32) 起步、
    /// 每次级联步进 48px 的对角线位置，若该位置仍与既有窗口几乎完全重叠则继续步进。
    pub fn place_new(
        &mut self,
        output_size: (i32, i32),
        window_size: (i32, i32),
        occupied: &[(i32, i32, i32, i32)],
    ) -> (i32, i32) {
        let margin = 32i32;
        let step = 48i32;
        let base = (margin, margin);
        let mut pos = base;
        for _ in 0..64 {
            let heavily_covered = occupied.iter().any(|&(x, y, w, h)| {
                let ix = (x + w).min(pos.0 + window_size.0) - x.max(pos.0);
                let iy = (y + h).min(pos.1 + window_size.1) - y.max(pos.1);
                let inter = ix.max(0) * iy.max(0);
                let area = (window_size.0.max(0) * window_size.1.max(0)).max(1);
                // 与某既有窗口重叠面积超过新窗口 90% 视为“被完全遮挡”
                inter * 10 >= area * 9
            });
            if !heavily_covered {
                break;
            }
            pos.0 += step;
            pos.1 += step;
            // 越界则回到起点并加大步进基准
            if pos.0 + window_size.0 > output_size.0 || pos.1 + window_size.1 > output_size.1 {
                self.cascade_step += 1;
                pos = (margin + self.cascade_step * 8, margin + self.cascade_step * 8);
            }
        }
        pos
    }
}

/// 网格排列：把 `count` 个窗口铺进 `area`（0..width × 0..height），
/// 返回每个窗口的 (x, y, w, h)，列数取近似正方形布局。
pub fn tile_grid(
    count: usize,
    area: (i32, i32),
    padding: i32,
) -> Vec<(i32, i32, i32, i32)> {
    if count == 0 {
        return Vec::new();
    }
    let cols = (count as f64).sqrt().ceil() as usize;
    let rows = count.div_ceil(cols);
    let cell_w = (area.0 - padding * (cols as i32 + 1)) / cols as i32;
    let cell_h = (area.1 - padding * (rows as i32 + 1)) / rows as i32;
    (0..count)
        .map(|i| {
            let col = i % cols;
            let row = i / cols;
            (
                padding + (col as i32) * (cell_w + padding),
                padding + (row as i32) * (cell_h + padding),
                cell_w,
                cell_h,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_avoids_full_cover() {
        let mut l = FloatingLayout::new();
        let occupied = vec![(32, 32, 800, 600)];
        let (x, y) = l.place_new((1920, 1080), (800, 600), &occupied);
        assert!((x, y) != (32, 32), "新窗口不应被完全叠在既有窗口上: ({x},{y})");
    }

    #[test]
    fn grid_covers_all() {
        let rects = tile_grid(5, (1920, 1080), 8);
        assert_eq!(rects.len(), 5);
        for (x, y, w, h) in &rects {
            assert!(*w > 0 && *h > 0);
            assert!(*x >= 0 && *y >= 0);
            assert!(x + w <= 1920 && y + h <= 1080);
        }
    }
}
