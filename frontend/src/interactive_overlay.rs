use std::collections::VecDeque;

use egui::{Color32, Context, Key, Pos2, Rect, StrokeKind, Vec2};

const GRID_SIZE: i32 = 20;
const TICK_MS: u64 = 150;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    x: i32,
    y: i32,
}

pub struct OverlayState {
    snake: VecDeque<Cell>,
    direction: Direction,
    next_direction: Direction,
    food: Cell,
    score: u32,
    last_tick: instant::Instant,
    game_over: bool,
    rng_state: u64,
}

impl OverlayState {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEED: AtomicU64 = AtomicU64::new(0xcafe_babe_dead_beef);

        let mut state = Self {
            snake: VecDeque::new(),
            direction: Direction::Right,
            next_direction: Direction::Right,
            food: Cell { x: 0, y: 0 },
            score: 0,
            last_tick: instant::Instant::now(),
            game_over: false,
            rng_state: SEED.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed),
        };

        let center = GRID_SIZE / 2;
        state.snake.push_back(Cell {
            x: center,
            y: center,
        });
        state.snake.push_back(Cell {
            x: center - 1,
            y: center,
        });
        state.snake.push_back(Cell {
            x: center - 2,
            y: center,
        });

        state.place_food();
        state
    }

    fn next_rng(&mut self) -> u64 {
        self.rng_state = self
            .rng_state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.rng_state
    }

    fn place_food(&mut self) {
        loop {
            let val = self.next_rng();
            let x = ((val >> 8) % GRID_SIZE as u64) as i32;
            let y = ((val >> 24) % GRID_SIZE as u64) as i32;
            let cell = Cell { x, y };
            if !self.snake.contains(&cell) {
                self.food = cell;
                return;
            }
        }
    }

    fn tick(&mut self) {
        if self.game_over {
            return;
        }

        let speed_ms = (TICK_MS as u32).saturating_sub(self.score * 5).max(50) as u64;
        if self.last_tick.elapsed() < std::time::Duration::from_millis(speed_ms) {
            return;
        }
        self.last_tick = instant::Instant::now();

        self.direction = self.next_direction;

        let head = *self.snake.front().unwrap();
        let new_head = match self.direction {
            Direction::Up => Cell {
                x: head.x,
                y: head.y - 1,
            },
            Direction::Down => Cell {
                x: head.x,
                y: head.y + 1,
            },
            Direction::Left => Cell {
                x: head.x - 1,
                y: head.y,
            },
            Direction::Right => Cell {
                x: head.x + 1,
                y: head.y,
            },
        };

        if new_head.x < 0 || new_head.x >= GRID_SIZE || new_head.y < 0 || new_head.y >= GRID_SIZE {
            self.game_over = true;
            return;
        }

        if self.snake.contains(&new_head) {
            self.game_over = true;
            return;
        }

        self.snake.push_front(new_head);

        if new_head == self.food {
            self.score += 1;
            self.place_food();
        } else {
            self.snake.pop_back();
        }
    }

    /// Update the overlay: handle input, tick game state, render.
    /// Returns `true` if the overlay should be closed.
    pub fn update(&mut self, ctx: &Context) -> bool {
        // Handle input
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            return true;
        }

        if !self.game_over {
            if ctx.input(|i| i.key_pressed(Key::ArrowUp)) && self.direction != Direction::Down {
                self.next_direction = Direction::Up;
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowDown)) && self.direction != Direction::Up {
                self.next_direction = Direction::Down;
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowLeft)) && self.direction != Direction::Right {
                self.next_direction = Direction::Left;
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowRight)) && self.direction != Direction::Left {
                self.next_direction = Direction::Right;
            }
        } else if ctx.input(|i| i.key_pressed(Key::Space)) {
            *self = Self::new();
        }

        self.tick();
        self.render(ctx);
        false
    }

    fn render(&self, ctx: &Context) {
        let screen_rect = ctx.content_rect();

        egui::Area::new(egui::Id::new("interactive_overlay"))
            .fixed_pos(screen_rect.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_min_size(screen_rect.size());

                let painter = ui.painter();

                // Darken background
                painter.rect_filled(
                    screen_rect,
                    0.0,
                    Color32::from_rgba_premultiplied(0, 0, 0, 220),
                );

                // Grid sizing
                let grid_pixels = screen_rect.height().min(screen_rect.width()) * 0.7;
                let cell_size = grid_pixels / GRID_SIZE as f32;
                let grid_origin = Pos2::new(
                    screen_rect.center().x - grid_pixels / 2.0,
                    screen_rect.center().y - grid_pixels / 2.0 + 20.0,
                );

                // Grid background
                let grid_rect = Rect::from_min_size(grid_origin, Vec2::splat(grid_pixels));
                painter.rect_filled(grid_rect, 4.0, Color32::from_rgb(20, 20, 30));
                painter.rect_stroke(
                    grid_rect,
                    4.0,
                    egui::Stroke::new(2.0_f32, Color32::from_rgb(60, 60, 80)),
                    StrokeKind::Inside,
                );

                // Subtle grid lines
                for i in 1..GRID_SIZE {
                    let x = grid_origin.x + i as f32 * cell_size;
                    let y = grid_origin.y + i as f32 * cell_size;
                    let grid_line = egui::Stroke::new(
                        0.5_f32,
                        Color32::from_rgba_premultiplied(40, 40, 60, 80),
                    );
                    painter.line_segment(
                        [
                            Pos2::new(x, grid_origin.y),
                            Pos2::new(x, grid_origin.y + grid_pixels),
                        ],
                        grid_line,
                    );
                    painter.line_segment(
                        [
                            Pos2::new(grid_origin.x, y),
                            Pos2::new(grid_origin.x + grid_pixels, y),
                        ],
                        grid_line,
                    );
                }

                // Food
                let food_rect = Rect::from_min_size(
                    Pos2::new(
                        grid_origin.x + self.food.x as f32 * cell_size,
                        grid_origin.y + self.food.y as f32 * cell_size,
                    ),
                    Vec2::splat(cell_size),
                );
                painter.rect_filled(food_rect.shrink(1.0), 2.0, Color32::from_rgb(255, 80, 80));

                // Snake
                for (i, cell) in self.snake.iter().enumerate() {
                    let cell_rect = Rect::from_min_size(
                        Pos2::new(
                            grid_origin.x + cell.x as f32 * cell_size,
                            grid_origin.y + cell.y as f32 * cell_size,
                        ),
                        Vec2::splat(cell_size),
                    );
                    let color = if i == 0 {
                        Color32::from_rgb(100, 255, 100)
                    } else {
                        Color32::from_rgb(60, 200, 60)
                    };
                    painter.rect_filled(cell_rect.shrink(1.0), 2.0, color);
                }

                // Score
                painter.text(
                    Pos2::new(screen_rect.center().x, grid_origin.y - 10.0),
                    egui::Align2::CENTER_BOTTOM,
                    format!("Score: {}", self.score),
                    egui::FontId::proportional(24.0),
                    Color32::WHITE,
                );

                if self.game_over {
                    painter.text(
                        screen_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "GAME OVER",
                        egui::FontId::proportional(48.0),
                        Color32::from_rgb(255, 80, 80),
                    );
                    painter.text(
                        Pos2::new(screen_rect.center().x, screen_rect.center().y + 40.0),
                        egui::Align2::CENTER_CENTER,
                        "SPACE to restart | ESC to close",
                        egui::FontId::proportional(16.0),
                        Color32::from_rgb(150, 150, 150),
                    );
                } else {
                    painter.text(
                        Pos2::new(screen_rect.center().x, grid_rect.max.y + 15.0),
                        egui::Align2::CENTER_TOP,
                        "Arrow keys to move | ESC to close",
                        egui::FontId::proportional(14.0),
                        Color32::from_rgb(100, 100, 100),
                    );
                }
            });

        ctx.request_repaint();
    }
}
