//! Platform window configuration; independent of the engine layer.

#[derive(Debug, Clone)]
pub struct WindowConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub min_width: Option<u32>,
    pub min_height: Option<u32>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub position_x: Option<i32>,
    pub position_y: Option<i32>,
    pub resizable: bool,
    pub fullscreen: bool,
    pub maximized: bool,
    pub visible: bool,
    pub decorations: bool,
    pub vsync: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "PrismaRev".into(),
            width: 1600,
            height: 900,
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            position_x: None,
            position_y: None,
            resizable: true,
            fullscreen: false,
            maximized: false,
            visible: true,
            decorations: true,
            vsync: true,
        }
    }
}
