use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug)]
pub enum Tile {
    Leaf(usize),
    Split {
        axis: Axis,
        percent: u16,
        first: Box<Tile>,
        second: Box<Tile>,
    },
}

impl Tile {
    pub fn rectangles(&self, rect: Rect, out: &mut Vec<(usize, Rect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, rect)),
            Self::Split {
                axis,
                percent,
                first,
                second,
            } => {
                let (a, b) = match axis {
                    Axis::Horizontal => {
                        let w = (rect.width as u32 * *percent as u32 / 100) as u16;
                        (
                            Rect::new(rect.x, rect.y, w, rect.height),
                            Rect::new(rect.x + w, rect.y, rect.width - w, rect.height),
                        )
                    }
                    Axis::Vertical => {
                        let h = (rect.height as u32 * *percent as u32 / 100) as u16;
                        (
                            Rect::new(rect.x, rect.y, rect.width, h),
                            Rect::new(rect.x, rect.y + h, rect.width, rect.height - h),
                        )
                    }
                };
                first.rectangles(a, out);
                second.rectangles(b, out);
            }
        }
    }

    pub fn split(&mut self, target: usize, new: usize, axis: Axis) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    percent: 50,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, axis) || second.split(target, new, axis)
            }
        }
    }

    pub fn remove(&mut self, target: usize) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if matches!(**first, Self::Leaf(id) if id == target) {
                    *self = (**second).clone();
                    true
                } else if matches!(**second, Self::Leaf(id) if id == target) {
                    *self = (**first).clone();
                    true
                } else {
                    first.remove(target) || second.remove(target)
                }
            }
        }
    }

    pub fn contains(&self, target: usize) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    pub fn resize(&mut self, target: usize, axis: Axis, delta: i16) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                axis: split_axis,
                percent,
                first,
                second,
            } => {
                if first.resize(target, axis, delta) || second.resize(target, axis, delta) {
                    return true;
                }
                if *split_axis == axis && (first.contains(target) || second.contains(target)) {
                    *percent = (*percent as i16 + delta).clamp(15, 85) as u16;
                    return true;
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_splits_cover_area_and_collapse() {
        let mut tile = Tile::Leaf(0);
        tile.split(0, 1, Axis::Horizontal);
        tile.split(1, 2, Axis::Vertical);
        let mut rects = Vec::new();
        tile.rectangles(Rect::new(0, 0, 101, 41), &mut rects);
        assert_eq!(
            rects
                .iter()
                .map(|(_, r)| r.width as u32 * r.height as u32)
                .sum::<u32>(),
            101 * 41
        );
        assert!(tile.remove(1));
        assert!(!tile.contains(1));
        assert!(tile.contains(0) && tile.contains(2));
    }
}
