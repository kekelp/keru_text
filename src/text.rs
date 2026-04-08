use crate::*;
#[cfg(feature = "accessibility")]
use accesskit::{NodeId, TreeUpdate};
use slotmap::{SlotMap, DefaultKey};
#[cfg(feature = "accessibility")]
use std::collections::HashMap;
use std::ops::DerefMut;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use winit::{event::{KeyEvent, Modifiers, MouseButton, WindowEvent}, keyboard::{Key, NamedKey}, window::Window};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use std::sync::{Arc, Weak};
use winit::window::WindowId;
use parley::{FontContext, LayoutContext};

const MULTICLICK_DELAY: f64 = 0.4;
const MULTICLICK_TOLERANCE_SQUARED: f64 = 26.0;

/// Direction for cross-box selection extension.
#[derive(Debug, Clone, Copy)]
enum SelectionDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowInfo {
    pub(crate) window_id: WindowId,
    pub(crate) dimensions: (f32, f32),
    pub(crate) prepared: bool,
    pub(crate) scale_factor: f64,
}

#[derive(Debug)]
pub(crate) struct StyleInner {
    pub(crate) text_style: TextStyle2,
    pub(crate) text_edit_style: TextEditStyle,
    pub(crate) version: u64,
}


/// Centralized struct that holds collections of [`TextBox`]es, [`TextEdit`]s, [`TextStyle2`]s.
pub struct Text {
    pub(crate) text_boxes: SlotMap<DefaultKey, TextBox>,
    pub(crate) text_edits: SlotMap<DefaultKey, TextEdit>,

    // Box to have a stable address for the backref pointers
    pub(crate) shared: Box<Shared>,

    pub(crate) style_version_id_counter: u64,

    pub(crate) input_state: TextInputState,

    pub(crate) mouse_hit_stack: Vec<(AnyBox, f32)>,

    pub(crate) using_frame_based_visibility: bool,

    pub(crate) scrolled_moved_indices: Vec<AnyBox>,
    pub(crate) scroll_animations: Vec<ScrollAnimation>,

    pub(crate) current_visibility_frame: u64,

    pub(crate) renderer: TextRenderer,

    #[cfg(feature = "accessibility")]
    pub(crate) accesskit_id_to_text_handle_map: HashMap<NodeId, AnyBox>,

    /// Internal buffer for collecting selected text across multiple boxes.
    selected_text_buffer: String,
}

/// Data that TextBoxMut and similar things need to have a reference to.
pub(crate) struct Shared {
    pub(crate) render_data: RenderData,

    pub styles: SlotMap<DefaultKey, StyleInner>,
    pub default_style_key: DefaultKey,
    pub rebuild_glyph_quad_buffer: bool,
    pub scrolled: bool,
    pub focused: Option<AnyBox>,

    /// Text boxes that are part of the current multi-box selection.
    /// When non-empty, selection rects should be drawn for all boxes in this list.
    pub multi_box_selection: Vec<DefaultKey>,

    /// The anchor point for cross-box selection: (box_key, local_x, local_y).
    /// Set when clicking on a text box, used when shift-clicking across linked boxes.
    pub cross_box_selection_anchor: Option<DefaultKey>,

    /// The box that currently holds the keyboard selection cursor for cross-box keyboard selection.
    /// None means cursor is in the focused/anchor box.
    pub cross_box_cursor_key: Option<DefaultKey>,

    pub windows: Vec<WindowInfo>,
    pub layout_cx: LayoutContext<ColorBrush>,
    pub font_cx: FontContext,

    pub rerender_cursor: bool,

    // Throttle paste to once per frame to avoid layout rebuild spam.
    pub pasted_this_frame: bool,

    #[cfg(feature = "accessibility")]
    pub accesskit_tree_update: TreeUpdate,
    #[cfg(feature = "accessibility")]
    pub accesskit_focus_tracker: FocusChange,
    pub current_event_number: u64,
    #[cfg(feature = "accessibility")]
    pub node_id_generator: fn() -> NodeId,

    // Cursor blink state
    pub cursor_blink_start: Option<Instant>,
    pub cursor_blink_animation_currently_visible: bool,
    pub cursor_blink_waker: Option<CursorBlinkWaker>,

    pub window: Option<Weak<Window>>,
}

impl Shared {
    pub(crate) fn update_blink_timer(&mut self) {
        if let Some(start_time) = self.cursor_blink_start {
            let elapsed = Instant::now().duration_since(start_time);
            let blink_period = Duration::from_millis(CURSOR_BLINK_TIME_MILLIS);
            let blinked_out = (elapsed.as_millis() / blink_period.as_millis()) % 2 == 0;
            let changed = blinked_out != self.cursor_blink_animation_currently_visible;

            self.cursor_blink_animation_currently_visible = blinked_out;

            if changed {
                self.rerender_cursor = true;
            }
        }
    }

    pub(crate) fn reset_cursor_blink(&mut self) {
        if let Some(AnyBox::TextEdit(_)) = self.focused {
            // todo: reorganize some stuff and also check that the selection is collapsed?
            self.cursor_blink_start = Some(Instant::now());
            self.cursor_blink_animation_currently_visible = true;
            self.rerender_cursor = true;

            if let Some(timer) = &self.cursor_blink_waker {
                timer.start();
            }
        } else {
            self.cursor_blink_start = None;
            if let Some(waker) = &self.cursor_blink_waker {
                waker.stop();
            }
        }
    }
    
    pub(crate) fn stop_cursor_blink(&mut self) {
        self.cursor_blink_start = None;
        if let Some(waker) = &self.cursor_blink_waker {
            waker.stop();
        }
    }
}

#[cfg(feature = "accessibility")]
pub(crate) struct FocusChange {
    new_focus: Option<NodeId>,
    old_focus: Option<NodeId>,
    event_number: u64,
}
#[cfg(feature = "accessibility")]
impl FocusChange {
    pub(crate) fn new() -> FocusChange {
        FocusChange { new_focus: None, old_focus: None, event_number: 0 }
    }
}

/// Handle for a text edit box.
/// 
/// Obtained when creating a text edit box with [`Text::add_text_edit()`].
/// 
/// Use with [`Text::get_text_edit()`] to get a reference to the corresponding [`TextEdit`]. 
#[derive(Debug)]
pub struct TextEditHandle {
    pub(crate) key: DefaultKey,
}

/// Cloneable handle for a text edit box.
/// 
/// Use with [`Text::try_get_text_edit()`] to get an optional reference to the corresponding [`TextBox`].
/// 
/// Because this handle is not unique, the text box that it refers to can be removed while the handle is still live. This is why [`Text::try_get_text_edit()`] returns an `Option`.
#[derive(Debug, Clone, Copy)]
pub struct ClonedTextEditHandle {
    pub(crate) key: DefaultKey,
}

/// Handle for a text box.
/// 
/// Obtained when creating a text box with [`Text::add_text_box()`].
/// 
/// Use with [`Text::get_text_box()`] to get a reference to the corresponding [`TextBox`].
#[derive(Debug)]
pub struct TextBoxHandle {
    pub(crate) key: DefaultKey,
}

/// Cloneable handle for a text box.
/// 
/// Use with [`Text::try_get_text_box()`] to get an optional reference to the corresponding [`TextBox`].
/// 
/// Because this handle is not unique, the text box that it refers to can be removed while the handle is still live. This is why [`Text::try_get_text_box()`] returns an `Option`.
#[derive(Debug, Clone, Copy)]
pub struct ClonedTextBoxHandle {
    pub(crate) key: DefaultKey,
}

impl TextBoxHandle {
    /// Get a non-unique handle cloned handle from this handle.
    pub fn to_cloned(&self) -> ClonedTextBoxHandle {
        ClonedTextBoxHandle { key: self.key }
    }
}

impl TextEditHandle {
    /// Get a non-unique handle cloned handle from this handle.
    pub fn to_cloned(&self) -> ClonedTextEditHandle {
        ClonedTextEditHandle { key: self.key }
    }
}


#[cfg(feature = "panic_on_handle_drop")]
impl Drop for TextEditHandle {
    fn drop(&mut self) {
        panic!(
            "TextEditHandle was dropped without being consumed! \
            This means that the corresponding text edit wasn't removed. To avoid leaking it, you should call Text::remove_text_edit(handle). \
            If you're intentionally leaking this text edit, you can use \
            std::mem::forget(handle) to skip the handle's drop() call and avoid this panic. \
            You can also disable this check by disabling the \"panic_on_handle_drop\" feature in Cargo.toml."
        );
    }
}

#[cfg(feature = "panic_on_handle_drop")]
impl Drop for TextBoxHandle {
    fn drop(&mut self) {
        panic!(
            "TextBoxHandle was dropped without being consumed! \
            This means that the corresponding text box wasn't removed. To avoid leaking it, you should call Text::remove_text_box(handle). \
            If you're intentionally leaking this text box, you can use \
            std::mem::forget(handle) to skip the handle's drop() call and avoid this panic. \
            You can also disable this check by disabling the \"panic_on_handle_drop\" feature in Cargo.toml."
        );
    }
}


/// Handle for a text style. Use with Text methods to apply styles to text.
#[derive(Debug, Clone, Copy)]
pub struct StyleHandle {
    pub(crate) key: DefaultKey,
}
impl StyleHandle {
    pub(crate) fn sneak_clone(&self) -> Self {
        Self { key: self.key }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LastClickInfo {
    pub(crate) time: Instant,
    pub(crate) pos: (f64, f64),
    pub(crate) focused: Option<AnyBox>,
}

#[derive(Debug, Clone)]
pub(crate) struct MouseState {
    pub pointer_down: bool,
    pub cursor_pos: (f64, f64),
    pub last_click_info: Option<LastClickInfo>,
    pub click_count: u32,
}

impl MouseState {
    pub fn new() -> Self {
        Self {
            pointer_down: false,
            cursor_pos: (0.0, 0.0),
            last_click_info: None,
            click_count: 0,
        }
    }
}

/// A non-owning reference to either a `TextBox` or a `TextEditBox`.
/// 
///[`TextBoxHandle`] and [`TextEditHandle`] can be converted into `AnyBox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnyBox {
    /// Text edit box
    TextEdit(DefaultKey),
    /// Text box
    TextBox(DefaultKey),
}

pub(crate) trait IntoAnyBox {
    fn get_anybox(&self) -> AnyBox;
}
impl IntoAnyBox for TextBoxHandle {
    fn get_anybox(&self) -> AnyBox {
        AnyBox::TextBox(self.key)
    }
}
impl IntoAnyBox for TextEditHandle {
    fn get_anybox(&self) -> AnyBox {
        AnyBox::TextEdit(self.key)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TextInputState {
    pub(crate) mouse: MouseState,
    pub(crate) modifiers: Modifiers,
}

impl TextInputState {
    pub fn new() -> Self {
        Self {
            mouse: MouseState::new(),
            modifiers: Modifiers::default(),
        }
    }

    pub fn handle_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = *modifiers;
            }
            WindowEvent::CursorMoved { position, .. } => {
                let cursor_pos = (position.x, position.y);
                self.mouse.cursor_pos = cursor_pos;
            },

            WindowEvent::MouseInput { state, .. } => {
                self.mouse.pointer_down = state.is_pressed();
            },
            _ => {}
        }
    }
}

fn apply_shift_nav_op(
    selection: &mut parley::Selection,
    layout: &parley::Layout<ColorBrush>,
    event: &KeyEvent,
    action_mod: bool,
) -> Option<bool> {
    match &event.logical_key {
        Key::Named(NamedKey::ArrowLeft) => {
            if action_mod { selection.select_word_left(layout); } else { selection.select_left(layout); }
            Some(false)
        }
        Key::Named(NamedKey::ArrowRight) => {
            if action_mod { selection.select_word_right(layout); } else { selection.select_right(layout); }
            Some(true)
        }
        Key::Named(NamedKey::ArrowUp) => {
            selection.select_up(layout);
            Some(false)
        }
        Key::Named(NamedKey::ArrowDown) => {
            selection.select_down(layout);
            Some(true)
        }
        Key::Named(NamedKey::Home) => {
            selection.select_to_line_start(layout);
            Some(false)
        }
        Key::Named(NamedKey::End) => {
            selection.select_to_line_end(layout);
            Some(true)
        }
        _ => None,
    }
}


impl Text {
    /// Create a new Text instance with a GPU renderer.
    pub fn new(device: &Device, queue: &Queue, format: TextureFormat) -> Self {
        Self::new_with_params(device, queue, format, None, TextRendererParams::default())
    }

    /// Create a new Text instance with custom renderer parameters.
    pub fn new_with_params(
        device: &Device,
        queue: &Queue,
        format: TextureFormat,
        depth_stencil: Option<DepthStencilState>,
        params: TextRendererParams,
    ) -> Self {
        let mut styles = SlotMap::with_capacity_and_key(10);
        let default_style_key = styles.insert(StyleInner {
            text_style: original_default_style(),
            text_edit_style: TextEditStyle::default(),
            version: 0,
        });

        let atlas_size = params.atlas_page_size.size(device);
        let mut render_data = RenderData::new(device, atlas_size);
        render_data.set_srgb(format.is_srgb());

        let renderer = TextRenderer::new_with_params(
            device.clone(),
            queue.clone(),
            format,
            depth_stencil,
            atlas_size,
            &render_data.box_data,
            &render_data.group_transforms,
            &render_data.glyph_quads,
        );

        Self {
            text_boxes: SlotMap::with_capacity(10),
            text_edits: SlotMap::with_capacity(10),
            style_version_id_counter: 0,
            input_state: TextInputState::new(),
            mouse_hit_stack: Vec::with_capacity(6),
            scrolled_moved_indices: Vec::new(),
            scroll_animations: Vec::new(),
            current_visibility_frame: 1,
            using_frame_based_visibility: false,
            renderer,

            #[cfg(feature = "accessibility")]
            accesskit_id_to_text_handle_map: HashMap::with_capacity(50),

            selected_text_buffer: String::with_capacity(25),

            shared: Box::new(Shared {
                render_data,
                windows: Vec::with_capacity(1),
                styles,
                default_style_key,
                rebuild_glyph_quad_buffer: true,
                scrolled: true,
                focused: None,
                multi_box_selection: Vec::new(),
                cross_box_selection_anchor: None,
                cross_box_cursor_key: None,
                layout_cx: LayoutContext::new(),
                font_cx: FontContext::new(),
                rerender_cursor: false,
                pasted_this_frame: false,
                #[cfg(feature = "accessibility")]
                accesskit_focus_tracker: FocusChange::new(),
                current_event_number: 1,
                #[cfg(feature = "accessibility")]
                node_id_generator: crate::accessibility::next_node_id,
                #[cfg(feature = "accessibility")]
                accesskit_tree_update: TreeUpdate {
                    nodes: Vec::new(),
                    tree: None,
                    focus: NodeId(0),
                },
                cursor_blink_start: None,
                cursor_blink_animation_currently_visible: false,
                cursor_blink_waker: None,
                window: None,
            }),
        }
    }

    /// Setup automatic cursor blink wakeup for applications that pause their event loops.
    ///
    /// `window` is used to wake up the `winit` event loop automatically when it needs to redraw a blinking cursor.
    /// It is also used to enable/disable IME when a text edit box gains or loses focus.
    ///
    /// In applications that don't pause their event loops, like games, there is no need to call this method.
    ///
    /// You can also handle cursor wakeups manually in your winit event loop with winit's `ControlFlow::WaitUntil` and [`Text::time_until_next_cursor_blink`]. See the `event_loop_smart.rs` example.
    pub fn set_auto_wakeup(&mut self, window: Arc<Window>) {
        self.shared.cursor_blink_waker = Some(CursorBlinkWaker::new(Arc::downgrade(&window)));
        self.shared.window = Some(Arc::downgrade(&window));
    }

    /// Load all the renderer data to the gpu.
    ///
    /// Useful only for custom rendering.
    pub fn load_to_gpu(&mut self) {
        let box_data_reallocated = self.shared.render_data.box_data.load_to_gpu(
            &self.renderer.device,
            &self.renderer.queue,
            "box data buffer",
        );
        let group_transforms_reallocated = self.shared.render_data.group_transforms.load_to_gpu(
            &self.renderer.device,
            &self.renderer.queue,
            "group transform buffer",
        );
        let mut needs_bind_group_recreate = box_data_reallocated || group_transforms_reallocated;

        // Update uniform buffer if needed
        if self.shared.render_data.needs_params_sync {
            let bytes: &[u8] = bytemuck::cast_slice(std::slice::from_ref(&self.shared.render_data.params));
            self.renderer.queue.write_buffer(&self.renderer.params_buffer, 0, bytes);
            self.shared.render_data.needs_params_sync = false;
        }

        // Rebuild texture arrays if needed
        if self.shared.render_data.needs_texture_array_rebuild {
            self.renderer.rebuild_texture_arrays(&mut self.shared.render_data);
            self.shared.render_data.needs_texture_array_rebuild = false;
            needs_bind_group_recreate = true;
        } else {
            self.renderer.update_texture_arrays(&mut self.shared.render_data);
        }

        // Update cursor color if needed
        let glyph_quads = &mut self.shared.render_data.glyph_quads;
        if self.shared.rerender_cursor {
            if let Some(cursor_index) = self.shared.render_data.cursor_quad_index {
                let color = if self.shared.cursor_blink_animation_currently_visible {
                    CURSOR_COLOR
                } else {
                    0x00_00_00_00
                };
                glyph_quads.data[cursor_index].color = color;
            }
        }

        // Sync quads buffer
        if glyph_quads.dirty {
            // Normal sync
            let glyph_quads_reallocated = glyph_quads.load_to_gpu(
                &self.renderer.device,
                &self.renderer.queue,
            );
            if glyph_quads_reallocated {
                needs_bind_group_recreate = true;
            }
        } else if self.shared.rerender_cursor {
            // Do a small sync for just the blinking cursor
            if let Some(cursor_index) = self.shared.render_data.cursor_quad_index {
                let bytes: &[u8] = bytemuck::cast_slice(std::slice::from_ref(&glyph_quads.data[cursor_index]));
                let offset = (cursor_index * std::mem::size_of::<GlyphQuad>()) as u64;
                self.renderer.queue.write_buffer(&glyph_quads.buffer, offset, bytes);
            }
        }
        self.shared.rerender_cursor = false;

        if needs_bind_group_recreate {
            self.renderer.recreate_bind_group(&self.shared.render_data);
        }
    }

    /// Render all prepared text using the provided render pass.
    pub fn render(&mut self, pass: &mut RenderPass) {
        self.load_to_gpu();
        self.renderer.render(pass, &self.shared.render_data);
    }

    /// Get render statistics from the last frame.
    ///
    /// Call this after `prepare_all()` and `load_to_gpu()` (or `render()`) to see
    /// what work was done and whether the optimizations are working.
    ///
    /// Only available in debug builds.
    #[cfg(debug_assertions)]
    pub fn render_stats(&self) -> &RenderStats {
        self.shared.render_data.stats()
    }

    pub(crate) fn new_style_version(&mut self) -> u64 {
        self.style_version_id_counter += 1;
        self.style_version_id_counter
    }

    /// Add a text box and return a handle.
    /// 
    /// The handle can be used with [`Text::get_text_box()`] to get a reference to the [`TextBox`] that was added.
    /// 
    /// The [`TextBox`] must be manually removed by calling [`Text::remove_text_box()`].
    /// 
    /// `text` can be a `String`, a `&'static str`, or a `Cow<'static, str>`.
    #[must_use]
    pub fn add_text_box(&mut self, text: impl Into<Cow<'static, str>>, pos: (f64, f64), size: (f32, f32), depth: f32) -> TextBoxHandle {
        let shared_backref: NonNull<Shared> = NonNull::new(self.shared.deref_mut()).unwrap();
        let mut text_box = TextBox::new(text, pos, size, depth, self.shared.default_style_key, shared_backref);

        let box_data_i = self.shared.render_data.box_data.insert(BoxGpu::zeroed());
        text_box.render_data_info.box_index = box_data_i;

        text_box.last_frame_touched = self.current_visibility_frame;
        text_box.style_version = self.shared.styles[text_box.style.key].version;
        let key = self.text_boxes.insert(text_box);
        let handle = TextBoxHandle { key };
        // Fill in the local copy of the key.
        self.get_text_box_mut(&handle).key = key;
        return handle;
    }

    /// Add a text edit and return a handle.
    /// 
    /// The handle can be used with [`Text::get_text_edit()`] to get a reference to the [`TextEdit`] that was added.
    /// 
    /// The [`TextEdit`] must be manually removed by calling [`Text::remove_text_edit()`].
    #[must_use]
    pub fn add_text_edit(&mut self, text: String, pos: (f64, f64), size: (f32, f32), depth: f32) -> TextEditHandle {
        let shared_backref: NonNull<Shared> = NonNull::new(self.shared.deref_mut()).unwrap();
        let mut text_edit = TextEdit::new(text, pos, size, depth, self.shared.default_style_key, shared_backref);

        let box_data_i = self.shared.render_data.box_data.insert(BoxGpu::zeroed());
        text_edit.text_box.render_data_info.box_index = box_data_i;

        text_edit.text_box.last_frame_touched = self.current_visibility_frame;
        text_edit.text_box.style_version = self.shared.styles[text_edit.text_box.style.key].version;
        let key = self.text_edits.insert(text_edit);
        let handle = TextEditHandle { key };
        // Fill in the local copy of the key.
        self.get_text_edit_mut(&handle).text_box.key = key;
        return handle;
    }

    /// Add a text box for a specific window and return a handle.
    /// 
    /// This is the multi-window version of [`Text::add_text_box()`].
    /// Only use this when you have multiple windows and want to restrict this text box to a specific window.
    #[must_use]
    pub fn add_text_box_for_window(&mut self, text: impl Into<Cow<'static, str>>, pos: (f64, f64), size: (f32, f32), depth: f32, window_id: WindowId) -> TextBoxHandle {
        let shared_backref: NonNull<Shared> = NonNull::new(self.shared.deref_mut()).unwrap();
        let mut text_box = TextBox::new(text, pos, size, depth, self.shared.default_style_key, shared_backref);
        text_box.last_frame_touched = self.current_visibility_frame;
        text_box.style_version = self.shared.styles[text_box.style.key].version;
        text_box.window_id = Some(window_id);
        let key = self.text_boxes.insert(text_box);
        let handle = TextBoxHandle { key };
        // Fill in the local copy of the key.
        self.get_text_box_mut(&handle).key = key;
        return handle;
    }

    /// Add a text edit for a specific window and return a handle.
    /// 
    /// This is the multi-window version of [`Text::add_text_edit()`].
    /// Only use this when you have multiple windows and want to restrict this text edit to a specific window.
    #[must_use]
    pub fn add_text_edit_for_window(&mut self, text: String, pos: (f64, f64), size: (f32, f32), depth: f32, window_id: WindowId) -> TextEditHandle {
        let shared_backref: NonNull<Shared> = NonNull::new(self.shared.deref_mut()).unwrap();
        let mut text_edit = TextEdit::new(text, pos, size, depth, self.shared.default_style_key, shared_backref);
        text_edit.text_box.last_frame_touched = self.current_visibility_frame;
        // todo: isn't this always the default style key? 
        text_edit.text_box.style_version = self.shared.styles[text_edit.text_box.style.key].version;
        text_edit.text_box.window_id = Some(window_id);
        let key = self.text_edits.insert(text_edit);
        let handle = TextEditHandle { key };
        // Fill in the local copy of the key.
        self.get_text_edit_mut(&handle).text_box.key = key;
        return handle;
    }




    /// Get a mutable reference to a text edit.
    /// 
    /// `handle` is the handle that was returned when first creating the text edit with [`Text::add_text_edit()`] or similar functions.
    ///    
    /// This is a fast lookup operation that does not require any hashing.
    pub fn get_text_edit_mut(&mut self, handle: &TextEditHandle) -> &mut TextEdit {
        return &mut self.text_edits[handle.key];
    }

    /// Get a reference to a text edit.
    /// 
    /// `handle` is the handle that was returned when first creating the text edit with [`Text::add_text_edit()`] or similar functions.
    ///    
    /// This is a fast lookup operation that does not require any hashing.
    pub fn get_text_edit(&self, handle: &TextEditHandle) -> &TextEdit {
        return &self.text_edits[handle.key];
    }

    /// Returns a text edit if it exists, or `None` if it has been removed.
    pub fn try_get_text_edit(&self, handle: &ClonedTextEditHandle) -> Option<&TextEdit> {
        return self.text_edits.get(handle.key);
    }

    /// Returns a mutable text edit if it exists, or `None` if it has been removed.
    pub fn try_get_text_edit_mut(&mut self, handle: &ClonedTextEditHandle) -> Option<&mut TextEdit> {
        return self.text_edits.get_mut(handle.key);
    }

    /// Adds a new text style and returns a handle to it.
    #[must_use]
    pub fn add_style(&mut self, text_style: TextStyle2, text_edit_style: Option<TextEditStyle>) -> StyleHandle {
        let text_edit_style = text_edit_style.unwrap_or_default();
        let new_version = self.new_style_version();
        let key = self.shared.styles.insert(StyleInner {
            text_style,
            text_edit_style,
            version: new_version,
        });
        StyleHandle { key }
    }

    /// Returns a reference to the text style.
    pub fn get_text_style(&self, handle: &StyleHandle) -> &TextStyle2 {
        &self.shared.styles[handle.key].text_style
    }

    /// Returns a mutable reference to the text style.
    pub fn get_text_style_mut(&mut self, handle: &StyleHandle) -> &mut TextStyle2 {
        self.shared.styles[handle.key].version = self.new_style_version();
        self.shared.rebuild_glyph_quad_buffer = true;
        &mut self.shared.styles[handle.key].text_style
    }

    /// Returns a reference to the text edit style.
    pub fn get_text_edit_style(&self, handle: &StyleHandle) -> &TextEditStyle {
        &self.shared.styles[handle.key].text_edit_style
    }

    /// Returns a mutable reference to the text edit style.
    pub fn get_text_edit_style_mut(&mut self, handle: &StyleHandle) -> &mut TextEditStyle {
        self.shared.styles[handle.key].version = self.new_style_version();
        self.shared.rebuild_glyph_quad_buffer = true;
        &mut self.shared.styles[handle.key].text_edit_style
    }

    /// Returns a reference to the default text style.
    pub fn get_default_text_style(&self) -> &TextStyle2 {
        &self.shared.styles[self.shared.default_style_key].text_style
    }

    /// Returns a mutable reference to the default text style.
    pub fn get_default_text_style_mut(&mut self) -> &mut TextStyle2 {
        let default_style_key = self.shared.default_style_key;
        self.shared.styles[default_style_key].version = self.new_style_version();
        self.shared.rebuild_glyph_quad_buffer = true;
        &mut self.shared.styles[default_style_key].text_style
    }

    /// Returns a reference to the default text edit style.
    pub fn get_default_text_edit_style(&self) -> &TextEditStyle {
        &self.shared.styles[self.shared.default_style_key].text_edit_style
    }

    /// Returns a mutable reference to the default text edit style.
    pub fn get_default_text_edit_style_mut(&mut self) -> &mut TextEditStyle {
        let default_style_key = self.shared.default_style_key;
        self.shared.styles[default_style_key].version = self.new_style_version();
        self.shared.rebuild_glyph_quad_buffer = true;
        &mut self.shared.styles[default_style_key].text_edit_style
    }

    /// Returns the original default text style.
    pub fn original_default_style(&self) -> TextStyle2 {
        original_default_style()
    }

    /// Advance an internal global frame counter that causes all text boxes to be implicitly marked as outdated and hidden.
    /// 
    /// You can then use [`Text::refresh_text_box()`] to "refresh" only the text boxes that should stay visible.
    /// 
    /// This allows to control the visibility of text boxes in a more "declarative" way.
    pub fn advance_frame_and_hide_boxes(&mut self) {
        self.current_visibility_frame += 1;
        self.using_frame_based_visibility = true;
    }

    /// Refresh a text box, causing it to stay visible even if [`Text::advance_frame_and_hide_boxes()`] was called.
    /// 
    /// Part of the "declarative" interface.  
    pub fn refresh_text_box(&mut self, handle: &TextBoxHandle) {
        if let Some(text_box) = self.text_boxes.get_mut(handle.key) {
            text_box.last_frame_touched = self.current_visibility_frame;
        }
    }

    /// Refresh a text edit box, causing it to stay visible even if [`Text::advance_frame_and_hide_boxes()`] was called.
    /// 
    /// Part of the "declarative" interface.
    pub fn refresh_text_edit(&mut self, handle: &TextEditHandle) {
        if let Some(text_edit) = self.text_edits.get_mut(handle.key) {
            text_edit.text_box.last_frame_touched = self.current_visibility_frame;
        }
    }

    /// Remove a text box.
    /// 
    /// `handle` is the handle that was returned when first creating the text box with [`Text::add_text_box()`].
    pub fn remove_text_box(&mut self, handle: TextBoxHandle) {
        self.shared.rebuild_glyph_quad_buffer = true;
        if let Some(AnyBox::TextBox(key)) = self.shared.focused {
            if key == handle.key {
                self.shared.focused = None;
            }
        }
        
        // Remove from accessibility mapping if it exists
        #[cfg(feature = "accessibility")]
        if let Some(text_box) = self.text_boxes.get(handle.key) {
            if let Some(accesskit_id) = text_box.accesskit_id {
                self.accesskit_id_to_text_handle_map.remove(&accesskit_id);
            }
        }
        
        let text_box = self.text_boxes.remove(handle.key).unwrap();
        
        let box_data_i = text_box.render_data_info.box_index;
        self.shared.render_data.box_data.remove(box_data_i);

        std::mem::forget(handle);
    }


    /// Remove a text edit.
    /// 
    /// `handle` is the handle that was returned when first creating the text edit with [`Text::add_text_edit()`] or similar functions.
    pub fn remove_text_edit(&mut self, handle: TextEditHandle) {
        self.shared.rebuild_glyph_quad_buffer = true;
        if let Some(AnyBox::TextEdit(i)) = self.shared.focused {
            if i == handle.key {
                self.shared.focused = None;
            }
        }
        
        // Remove from accessibility mapping if it exists
        #[cfg(feature = "accessibility")]
        if let Some((_text_edit, text_box)) = self.text_edits.get(handle.key) {
            if let Some(accesskit_id) = text_box.accesskit_id {
                self.accesskit_id_to_text_handle_map.remove(&accesskit_id);
            }
        }
        
        let text_edit = self.text_edits.remove(handle.key).unwrap();

        let box_data_i = text_edit.text_box.render_data_info.box_index;
        self.shared.render_data.box_data.remove(box_data_i);

        std::mem::forget(handle);
    }

    /// Remove a text style.
    ///
    /// If any text boxes are set to this style, they will revert to the default style.
    pub fn remove_style(&mut self, handle: StyleHandle) {
        self.shared.styles.remove(handle.key);
    }

    /// Insert a group transform and return a handle.
    ///
    /// Group transforms can be shared across multiple text boxes and are applied
    /// after the per-box transform.
    #[must_use]
    pub fn insert_group_transform(&mut self, transform: GroupTransform) -> GroupTransformHandle {
        let index = self.shared.render_data.group_transforms.insert(transform);
        GroupTransformHandle(index)
    }

    /// Remove a group transform.
    ///
    /// Text boxes using this transform should have their group_transform_index cleared first.
    pub fn remove_group_transform(&mut self, handle: GroupTransformHandle) {
        self.shared.render_data.group_transforms.remove(handle.0);
    }

    /// Update a group transform.
    ///
    /// All text boxes using this transform will be affected.
    pub fn update_group_transform(&mut self, handle: GroupTransformHandle, transform: GroupTransform) {
        self.shared.render_data.group_transforms[handle.0] = transform;
    }

    /// Get the value of a group transform.
    pub fn get_group_transform(&self, handle: GroupTransformHandle) -> GroupTransform {
        self.shared.render_data.group_transforms[handle.0]
    }


    /// Layout and rasterize all text belonging to a window, prepare the render data.
    pub fn prepare_all_for_window(&mut self, window: &Window) {
        let window_id = window.id();
        let window_size = window.inner_size();
        let (width, height) = (window_size.width as f32, window_size.height as f32);

        self.prepare_all_impl(window_id, (width, height));
    }

    /// Layout and rasterize all text, prepare the render data.
    ///
    /// This function is for single-window applications only. For multi-window, use [`Text::prepare_all_for_window`].
    ///
    /// [`Text`] keeps track of all changes to text boxes internally. So this function can be called multiple times in same frame without issues, if needed.
    pub fn prepare_all(&mut self) {
        let res = self.shared.windows.first().map(|w| (w.window_id, w.dimensions));

        // This is what we would want to do:
        // let (window_id, window_size) = res.expect("Text::prepare_all didn't register any windows, are you calling Text::handle_events?");
        // However, it seems that winit continues to give us RedrawRequested events even after the CloseRequested event, even if we calling event_loop.exit()?
        // But we unregister windows on CloseRequested.
        // For this reason, it seems that we have to accept that prepare_all might be called when no windows are around and silently do nothing.
        let Some((window_id, window_size)) = res else {
            // Even if there are no windows, we should reset the change flags
            // so they don't stay stuck at true
            self.shared.rebuild_glyph_quad_buffer = false;
            return;
        };

        self.prepare_all_impl(window_id, window_size);
    }

    pub(crate) fn prepare_all_impl(&mut self, window_id: WindowId, window_size: (f32, f32)) {
        #[cfg(debug_assertions)] {
            self.shared.render_data.stats = RenderStats::default();
        }

        self.shared.pasted_this_frame = false;
        self.shared.render_data.update_resolution(window_size.0, window_size.1);

        // todo: an extra loop just for this?
        for (_key, text_edit) in self.text_edits.iter_mut() {
            text_edit.text_changed = false;
        }

        // todo: not sure if this works correctly with multi-window.
        if !self.shared.rebuild_glyph_quad_buffer && self.using_frame_based_visibility {
            // see if any text boxes were just hidden
            for (_i, text_edit) in self.text_edits.iter_mut() {
                if text_edit.text_box.last_frame_touched == self.current_visibility_frame - 1 {
                    self.shared.rebuild_glyph_quad_buffer = true;
                }
            }
            for (_i, text_box) in self.text_boxes.iter_mut() {
                if text_box.last_frame_touched == self.current_visibility_frame - 1 {
                    self.shared.rebuild_glyph_quad_buffer = true;
                }
            }
        }

        if self.shared.rebuild_glyph_quad_buffer {
            // Full clear and re-prepare everything
            self.shared.render_data.clear();
        } else if !self.scrolled_moved_indices.is_empty() {
            // Scroll only - just update BoxGpu, no clearing needed.
            if !self.handle_scroll_fast_path() {
                // Fast path failed (tolerance exceeded), fall back to full prepare
                self.shared.render_data.clear();
                self.shared.rebuild_glyph_quad_buffer = true;
            }
        }

        // Prepare text layout for all text boxes/edits (only if text_changed)
        if self.shared.rebuild_glyph_quad_buffer {
            let current_frame = self.current_visibility_frame;
            for (_key, text_edit) in self.text_edits.iter_mut() {
                if !text_edit.text_box.hidden() && text_edit.text_box.last_frame_touched == current_frame {
                    let should_render = text_edit.text_box.window_id.is_none() || text_edit.text_box.window_id == Some(window_id);
                    if should_render {
                        self.shared.render_data.prepare_text_edit_layout(text_edit);
                    }
                }
            }

            for (key, mut text_box) in self.text_boxes.iter_mut() {
                if !text_box.hidden() && text_box.last_frame_touched == current_frame {
                    let should_render = text_box.window_id.is_none() || text_box.window_id == Some(window_id);
                    if should_render {
                        let show_selection = self.shared.multi_box_selection.contains(&key);
                        self.shared.render_data.prepare_text_box_layout(&mut text_box, false, show_selection);
                    }
                }
            }
        }

        // Multi-window: mark prepared and check if all windows done.
        let should_clear_flags = {
            if let Some(window_info) = self.shared.windows.iter_mut().find(|info| info.window_id == window_id) {
                window_info.prepared = true;
            }
            self.shared.windows.iter().all(|info| info.prepared)
        };

        if should_clear_flags {
            self.clear_finished_scroll_animations();

            self.shared.rebuild_glyph_quad_buffer = false;
            self.using_frame_based_visibility = false;

            // Reset all windows to unprepared for next frame
            for window_info in &mut self.shared.windows {
                window_info.prepared = false;
            }

            self.shared.scrolled = self.get_max_animation_duration().is_some();
        }
    }

    /// Fast path for handling scroll-only changes by adjusting BoxGpu translation.
    /// Returns false if scroll has exceeded the tolerance from the base position (line culling),
    /// in which case the caller should fall back to a full re-prepare.
    fn handle_scroll_fast_path(&mut self) -> bool {
        for any_box in &self.scrolled_moved_indices {
            match any_box {
                AnyBox::TextEdit(i) => {
                    if let Some(text_edit) = self.text_edits.get_mut(*i) {
                        if text_edit.text_box.is_scroll_distance_above_tolerance() {
                            return false;
                        } else {
                            update_scroll(&mut self.shared.render_data, &mut text_edit.text_box.render_data_info, text_edit.text_box.scroll_offset);
                        }
                    }
                },
                AnyBox::TextBox(i) => {
                    if let Some(text_box) = self.text_boxes.get_mut(*i) {
                        if text_box.is_scroll_distance_above_tolerance() {
                            return false;
                        } else {
                            update_scroll(&mut self.shared.render_data, &mut text_box.render_data_info, text_box.scroll_offset);
                        }
                    }
                },
            }
        }
        true
    }

    /// Clear scroll indices only for elements that have finished their animations
    fn clear_finished_scroll_animations(&mut self) {
        self.scrolled_moved_indices.retain(|any_box| {
            match any_box {
                AnyBox::TextEdit(i) => {
                    // Keep in list if any animation is still running for this text edit
                    self.scroll_animations.iter().any(|anim| anim.handle.key == *i)
                },
                AnyBox::TextBox(_i) => {
                    // Text boxes don't have animations, so they can be cleared immediately
                    false
                },
            }
        });
    }

    /// Handle window events for all text areas in a specific window.
    /// 
    /// Returns `true` if the event was consumed by a text area.
    pub fn handle_event(&mut self, event: &WindowEvent, window: &Window) -> bool {
        let mut event_consumed = false;

        self.shared.current_event_number += 1;
        
        self.input_state.handle_event(event);

        // Register the window if not already there.
        // Only for a few events that should be  guaranteed to arrive for new windows, to avoid a lot of needless checks
        if let WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } = event {
            if self.shared.windows.iter().find(|w_info| w_info.window_id == window.id()).is_none() {
                self.shared.windows.push(WindowInfo { 
                    window_id: window.id(), 
                    dimensions: (window.inner_size().width as f32, window.inner_size().height as f32), 
                    prepared: false,
                    scale_factor: window.scale_factor(),
                });
            }
        }
        
        if let WindowEvent::Focused(focused) = event {
            if *focused {
                // self.shared.cursor_blink_animation_currently_visible = true;
                self.shared.reset_cursor_blink();
            } else {
                self.shared.cursor_blink_animation_currently_visible = false;
                // Should rerender to hide the selection rectangles
                self.shared.stop_cursor_blink();
                self.shared.rerender_cursor = true;
            }
        }

        if let WindowEvent::CloseRequested | WindowEvent::Destroyed = event {
            self.shared.windows.retain(|info| info.window_id != window.id());
        }

        if let WindowEvent::ScaleFactorChanged { scale_factor, inner_size_writer: _ } = event {
            let window = self.shared.windows.iter_mut().find(|info| info.window_id == window.id()).unwrap();
            window.scale_factor = *scale_factor;
        }

        if let WindowEvent::Resized(new_size) = event {
            let window = self.shared.windows.iter_mut().find(|info| info.window_id == window.id()).unwrap();
            window.dimensions = (new_size.width as f32, new_size.height as f32);
        }

        // update smooth scrolling animations
        if let WindowEvent::RedrawRequested = event {
            self.shared.update_blink_timer();

            let animation_updated = self.update_smooth_scrolling();
            if animation_updated {
                self.shared.scrolled = true;
            }
        }

        let mut handled_shift_click = false;
        if let WindowEvent::MouseInput { state, button, .. } = event {
            if state.is_pressed() && *button == MouseButton::Left {
                let new_focus = self.find_topmost_selectable_at_pos_for_window(self.input_state.mouse.cursor_pos, window.id());
                if new_focus.is_some() {
                    event_consumed = true;
                }
                handled_shift_click = self.handle_left_click(new_focus);
                self.handle_click_counting();
            }
        }

        if let WindowEvent::MouseWheel { .. } = event {
            let hovered = self.find_topmost_selectable_at_pos_for_window(self.input_state.mouse.cursor_pos, window.id());
            if let Some(hovered_widget) = hovered {
                let consumed = self.handle_scroll_event(hovered_widget, event, window);
                event_consumed |= consumed;
            }
        }

        if !handled_shift_click {
            if let Some(focused) = self.shared.focused {
                // Only handle the event if the focused element belongs to this window
                let focused_belongs_to_window = match focused {
                    AnyBox::TextEdit(i) => {
                        if let Some(text_edit) = self.text_edits.get(i) {
                            text_edit.text_box.window_id.is_none() || text_edit.text_box.window_id == Some(window.id())
                        } else {
                            false
                        }
                    },
                    AnyBox::TextBox(i) => {
                        if let Some(text_box) = self.text_boxes.get(i) {
                            text_box.window_id.is_none() || text_box.window_id == Some(window.id())
                        } else {
                            false
                        }
                    },
                };

                if focused_belongs_to_window {
                    let consumed = self.handle_focused_event(focused, event, window);
                    event_consumed |= consumed;

                    #[cfg(feature = "accessibility")] {
                        // todo: not the best, this includes decoration changes and stuff.
                        if self.need_rerender() {
                            self.push_ak_update_for_focused(focused);
                        }
                    }
                }
            }
        }

        return event_consumed;
    }

    fn find_topmost_selectable_at_pos_for_window(&mut self, cursor_pos: (f64, f64), window_id: WindowId) -> Option<AnyBox> {
        self.mouse_hit_stack.clear();

        // Find all text widgets at this position that belong to this window
        for (i, ed) in self.text_edits.iter_mut() {
            if ! ed.text_box.selectable { continue };
            if !ed.text_box.hidden && ed.text_box.last_frame_touched == self.current_visibility_frame && ed.text_box.hit_full_rect(cursor_pos) {
                // Only consider if this text edit belongs to this window (or has no window restriction)
                if ed.text_box.window_id.is_none() || ed.text_box.window_id == Some(window_id) {
                    self.mouse_hit_stack.push((AnyBox::TextEdit(i), ed.text_box.depth));
                }
            }
        }
        for (i, text_box) in self.text_boxes.iter_mut() {
            if ! text_box.selectable { continue };
            if !text_box.hidden && text_box.last_frame_touched == self.current_visibility_frame && text_box.hit_bounding_box(cursor_pos) {
                // Only consider if this text box belongs to this window (or has no window restriction)
                if text_box.window_id.is_none() || text_box.window_id == Some(window_id) {
                    self.mouse_hit_stack.push((AnyBox::TextBox(i), text_box.depth));
                }
            }
        }

        // Find the topmost (lowest depth value)
        let mut topmost = None;
        let mut top_z = f32::MAX;
        for (id, z) in self.mouse_hit_stack.iter() {
            if *z < top_z {
                top_z = *z;
                topmost = Some(*id);
            }
        }

        topmost
    }

    #[cfg(feature = "accessibility")]
    fn get_accesskit_id(&mut self, i: AnyBox) -> Option<NodeId> {
        return match i {
            AnyBox::TextEdit(i) => {
                let handle = TextEditHandle { key: i };
                let text_edit = get_full_text_edit_partial_borrows(&mut self.text_edits, &mut self.shared, &handle);
                text_edit.accesskit_id()
            },
            AnyBox::TextBox(i) => {
                let handle = TextBoxHandle { key: i };
                let text_box = get_full_text_box_partial_borrows(&mut self.text_boxes, &mut self.shared, &handle);
                text_box.accesskit_id()
            },
        }
    }

    /// Find the topmost text box that would receive mouse events, if it wasn't occluded by any non-text-box objects.
    /// 
    /// Returns the handle of the topmost text widget at the event position, or None if no widget is hit.
    /// Use this with [`Text::handle_event_with_topmost()`] for complex z-ordering scenarios.
    pub fn find_topmost_text_box(&mut self, event: &WindowEvent) -> Option<AnyBox> {
        // Only handle mouse events that have a position
        let cursor_pos = match event {
            WindowEvent::MouseInput { .. } => self.input_state.mouse.cursor_pos,
            WindowEvent::CursorMoved { position, .. } => (position.x, position.y),
            _ => return None,
        };

        self.find_topmost_at_pos(cursor_pos)
    }

    /// Get the depth of a text box by its handle.
    /// 
    /// Used for comparing depths when integrating with other objects that might occlude text boxs.
    pub fn get_text_box_depth(&self, text_box_id: &AnyBox) -> f32 {
        match text_box_id {
            AnyBox::TextEdit(i) => self.text_edits.get(*i).map(|te| te.text_box.depth).unwrap_or(f32::MAX),
            AnyBox::TextBox(i) => self.text_boxes.get(*i).map(|tb| tb.depth).unwrap_or(f32::MAX),
        }
    }

    /// Handle window events with a pre-determined topmost text box.
    /// 
    /// Use this in cases where text boxes might be occluded by other objects.
    /// Pass `Some(text_box_id)` if a text box should receive the event, or `None` if it's occluded.
    /// 
    /// If the text box is occluded, this function should still be called with `None`, so that other text boxes can defocus.
    pub fn handle_event_with_topmost(&mut self, event: &WindowEvent, window: &Window, topmost_text_box: Option<AnyBox>) {        
        // todo: add "consumed" here 
        self.input_state.handle_event(event);

        // update smooth scrolling animations
        if let WindowEvent::RedrawRequested = event {
            let animation_updated = self.update_smooth_scrolling();
            if animation_updated {
                window.request_redraw();
            }
        }

        let mut handled_shift_click = false;
        if let WindowEvent::MouseInput { state, button, .. } = event {
            if state.is_pressed() && *button == MouseButton::Left {
                handled_shift_click = self.handle_left_click(topmost_text_box);
                self.handle_click_counting();
            }
        }

        if let WindowEvent::MouseWheel { .. } = event {
            if let Some(hovered_widget) = topmost_text_box {
                self.handle_scroll_event(hovered_widget, event, window);
            }
        }

        if !handled_shift_click {
            if let Some(focused) = self.shared.focused {
                self.handle_focused_event(focused, event, window);
            }
        }
    }

    fn find_topmost_at_pos(&mut self, cursor_pos: (f64, f64)) -> Option<AnyBox> {
        self.mouse_hit_stack.clear();

        // Find all text widgets at this position
        for (i, te) in self.text_edits.iter_mut() {
            if !te.text_box.hidden && te.text_box.last_frame_touched == self.current_visibility_frame && te.text_box.hit_full_rect(cursor_pos) {
                self.mouse_hit_stack.push((AnyBox::TextEdit(i), te.text_box.depth));
            }
        }
        for (i, text_box) in self.text_boxes.iter_mut() {
            if !text_box.hidden && text_box.last_frame_touched == self.current_visibility_frame && text_box.hit_bounding_box(cursor_pos) {
                self.mouse_hit_stack.push((AnyBox::TextBox(i), text_box.depth));
            }
        }

        // Find the topmost (lowest depth value)
        let mut topmost = None;
        let mut top_z = f32::MAX;
        for (id, z) in self.mouse_hit_stack.iter() {
            if *z < top_z {
                top_z = *z;
                topmost = Some(*id);
            }
        }

        topmost
    }

    fn refocus(&mut self, new_focus: Option<AnyBox>) {
        let focus_changed = new_focus != self.shared.focused;

        if focus_changed {
            if let Some(old_focus) = self.shared.focused {
                self.remove_focus(old_focus);
            }

            // Clear multi-box selection and cross-box state when focus changes
            self.shared.multi_box_selection.clear();
            self.shared.cross_box_selection_anchor = None;
            self.shared.cross_box_cursor_key = None;

            // If the new focus is a TextBox, add it to multi_box_selection
            if let Some(AnyBox::TextBox(key)) = new_focus {
                self.shared.multi_box_selection.push(key);
            }

            #[cfg(feature = "accessibility")]
            {
                let new_focus_ak_id = new_focus.and_then(|new_focus| self.get_accesskit_id(new_focus));
                let old_focus_ak_id = self.shared.focused.and_then(|old_focus| self.get_accesskit_id(old_focus));
                self.shared.accesskit_focus_tracker.new_focus = new_focus_ak_id;
                self.shared.accesskit_focus_tracker.old_focus = old_focus_ak_id;
                self.shared.accesskit_focus_tracker.event_number = self.shared.current_event_number;
            }

            // Enable/disable IME based on whether a text edit is focused
            // Todo: what if the user wants to do his own IME stuff?
            if let Some(weak_window) = &self.shared.window {
                if let Some(window) = weak_window.upgrade() {
                    let ime_allowed = matches!(new_focus, Some(AnyBox::TextEdit(_)));
                    window.set_ime_allowed(ime_allowed);
                }
            }
        }

        self.shared.focused = new_focus;

        if focus_changed {
            // todo: could skip some rerenders here if the old focus wasn't editable and had collapsed selection.
            self.shared.rebuild_glyph_quad_buffer = true;
            self.shared.reset_cursor_blink();
        }
    }

    fn handle_click_counting(&mut self) {
        let now = Instant::now();
        let current_pos = self.input_state.mouse.cursor_pos;
        
        if let Some(last_info) = self.input_state.mouse.last_click_info.take() {
            if now.duration_since(last_info.time).as_secs_f64() < MULTICLICK_DELAY 
                && last_info.focused == self.shared.focused {
                let dx = current_pos.0 - last_info.pos.0;
                let dy = current_pos.1 - last_info.pos.1;
                let distance_squared = dx * dx + dy * dy;
                if distance_squared <= MULTICLICK_TOLERANCE_SQUARED {
                    self.input_state.mouse.click_count = (self.input_state.mouse.click_count + 1) % 4;
                } else {
                    self.input_state.mouse.click_count = 1;
                }
            } else {
                self.input_state.mouse.click_count = 1;
            }
        } else {
            self.input_state.mouse.click_count = 1;
        }
        
        self.input_state.mouse.last_click_info = Some(LastClickInfo {
            time: now,
            pos: current_pos,
            focused: self.shared.focused,
        });
    }
    
    fn remove_focus(&mut self, old_focus: AnyBox) {
        match old_focus {
            AnyBox::TextEdit(i) => {
                let handle = TextEditHandle { key: i };
                let text_edit = self.get_text_edit_mut(&handle);
                text_edit.text_box.reset_selection();
                self.shared.cursor_blink_animation_currently_visible = false;
            },
            AnyBox::TextBox(i) => {
                let handle = TextBoxHandle { key: i };
                self.get_text_box_mut(&handle).reset_selection();
            },
        }
    }
    
    fn handle_scroll_event(&mut self, hovered: AnyBox, event: &WindowEvent, window: &Window) -> bool {
        // scroll wheel event
        if let WindowEvent::MouseWheel { .. } = event {
            match hovered {
                AnyBox::TextEdit(i) => {
                    let handle = TextEditHandle { key: i };
                    let did_scroll = self.handle_text_edit_scroll_event(&handle, event, window);
                    if did_scroll {
                        self.scrolled_moved_indices.push(AnyBox::TextEdit(i));
                        self.shared.rerender_cursor = true;
                        self.shared.scrolled = true;
                    }
                    return did_scroll;
                },
                AnyBox::TextBox(_) => {}
            }
        }
        false
    }

    fn handle_focused_event(&mut self, focused: AnyBox, event: &WindowEvent, window: &Window) -> bool {
        if let WindowEvent::KeyboardInput { event, .. } = event {
            if event.state.is_pressed() {
                let mods_state = self.input_state.modifiers.state();
                let action_mod = if cfg!(target_os = "macos") {
                    mods_state.super_key()
                } else {
                    mods_state.control_key()
                };
                let shift = mods_state.shift_key();

                if shift {
                    match focused {
                        AnyBox::TextBox(i) => {
                            if self.handle_keyboard_selection(i, event, action_mod) {
                                return true;
                            }
                        }
                        AnyBox::TextEdit(i) => {
                            let can_handle = self.text_edits.get(i).map_or(false, |te| {
                                !te.text_box.hidden() && !te.disabled() && !te.showing_placeholder && !te.is_composing()
                            });
                            if can_handle {
                                let te = self.text_edits.get_mut(i).unwrap();
                                let tb = &mut te.text_box;
                                if apply_shift_nav_op(&mut tb.selection, &tb.layout, event, action_mod).is_some() {
                                    tb.shared_mut().rebuild_glyph_quad_buffer = true;
                                    return true;
                                }
                            }
                        }
                    }
                }

                if action_mod && !shift {
                    if let Key::Character(c) = event.key_without_modifiers() {
                        if c.as_str() == "c" {
                            match focused {
                                AnyBox::TextBox(_) => {
                                    if let Some(text) = self.selected_text() {
                                        with_clipboard(|cb| { cb.set_text(text).ok(); });
                                    }
                                }
                                AnyBox::TextEdit(i) => {
                                    if let Some(te) = self.text_edits.get(i) {
                                        if let Some(text) = te.text_box.selected_text() {
                                            with_clipboard(|cb| { cb.set_text(text).ok(); });
                                        }
                                    }
                                }
                            }
                            return true;
                        }
                    }
                }
            }
        }

        // todo: copying this for now, but maybe it can go into Shared
        let input_state = self.input_state.clone();

        match focused {
            AnyBox::TextEdit(i) => {
                let handle = TextEditHandle { key: i };
                let consumed = self.get_text_edit_mut(&handle).handle_event_editable(event, window, &input_state);

                if self.shared.rebuild_glyph_quad_buffer {
                    self.shared.reset_cursor_blink();
                }
                if !self.shared.rebuild_glyph_quad_buffer && self.shared.scrolled {
                    self.scrolled_moved_indices.push(AnyBox::TextEdit(i));
                }
                consumed
            },
            AnyBox::TextBox(i) => {
                let handle = TextBoxHandle { key: i };
                let consumed = self.get_text_box_mut(&handle).handle_event(event, window, &input_state);

                if !self.shared.rebuild_glyph_quad_buffer && self.shared.scrolled {
                    self.scrolled_moved_indices.push(AnyBox::TextBox(i));
                }

                // Handle cross-box selection extension for linked boxes
                if let WindowEvent::CursorMoved { .. } = event {
                    if input_state.mouse.pointer_down {
                        self.handle_cross_box_selection_extend(i);
                    }
                }

                consumed
            },
        }
    }

    /// Handle extending selection across linked text boxes when dragging.
    fn handle_cross_box_selection_extend(&mut self, focused_key: DefaultKey) {
        // Reset all extended selections first, they'll be recreated as needed
        for &key in &self.shared.multi_box_selection {
            if key != focused_key {
                self.text_boxes[key].selection = parley::Selection::default();
            }
        }
        self.shared.multi_box_selection.retain(|&key| key == focused_key);

        let did_extend_forward = self.extend_selection_in_direction(focused_key, SelectionDirection::Forward);
        let did_extend_backward = self.extend_selection_in_direction(focused_key, SelectionDirection::Backward);

        if did_extend_forward || did_extend_backward {
            self.shared.rebuild_glyph_quad_buffer = true;
        }
    }

    /// Extend selection in a given direction (forward to next_box, backward to prev_box).
    /// Returns true if any extension happened.
    fn extend_selection_in_direction(&mut self, focused_key: DefaultKey, direction: SelectionDirection) -> bool {
        let cursor_pos = self.input_state.mouse.cursor_pos;

        let anchor_point;
        let extend_point;
        match direction {
            SelectionDirection::Forward => {
                anchor_point = (0.0, 0.0);
                extend_point = (f32::MAX, f32::MAX);
            },
            SelectionDirection::Backward => {
                anchor_point = (f32::MAX, f32::MAX);
                extend_point = (0.0, 0.0);
            },
        };

        let focused_anchor_base = self.text_boxes[focused_key].selection.anchor_base();
        let mut current_key = focused_key;
        let mut did_extend = false;

        loop {
            // Check if cursor is past the boundary of current box
            let is_past_boundary = match direction {
                SelectionDirection::Forward => self.text_boxes[current_key].is_cursor_past_end(cursor_pos),
                SelectionDirection::Backward => self.text_boxes[current_key].is_cursor_before_start(cursor_pos),
            };

            if !is_past_boundary {
                break;
            }

            let linked_key = match direction {
                SelectionDirection::Forward => self.text_boxes[current_key].next_box,
                SelectionDirection::Backward => self.text_boxes[current_key].prev_box,
            };

            // Extend current box's selection to the boundary
            {
                let current_box = &mut self.text_boxes[current_key];

                // For non-focused boxes (which were reset), set anchor at opposite boundary first
                if current_key != focused_key {
                    current_box.selection.move_to_point(&current_box.layout, anchor_point.0, anchor_point.1);
                }

                current_box.selection.extend_selection_to_point(&current_box.layout, extend_point.0, extend_point.1);
            }
            did_extend = true;

            let Some(linked_key) = linked_key else {
                break;
            };

            // Check if cursor actually hits the linked box
            let cursor_hits_linked = self.text_boxes[linked_key].hit_full_rect(cursor_pos);

            if cursor_hits_linked {
                // Add linked box to multi_box_selection and set partial selection
                if !self.shared.multi_box_selection.contains(&linked_key) {
                    self.shared.multi_box_selection.push(linked_key);
                }

                let linked_box = &mut self.text_boxes[linked_key];
                let inv_transform = linked_box.transform().inverse().unwrap_or(Transform2D::identity());
                let local_pos = inv_transform.transform_point(euclid::Point2D::new(cursor_pos.0 as f32, cursor_pos.1 as f32));
                let local_cursor = (
                    local_pos.x + linked_box.scroll_offset.0,
                    local_pos.y + linked_box.scroll_offset.1,
                );

                match focused_anchor_base {
                    parley::AnchorBase::Word(_, _) => {
                        linked_box.selection.select_word_at_point(&linked_box.layout, anchor_point.0, anchor_point.1);
                    }
                    parley::AnchorBase::Line(_, _) => {
                        linked_box.selection.select_line_at_point(&linked_box.layout, anchor_point.0, anchor_point.1);
                    }
                    _ => {
                        linked_box.selection.move_to_point(&linked_box.layout, anchor_point.0, anchor_point.1);
                    }
                }
                linked_box.selection.extend_selection_to_point(&linked_box.layout, local_cursor.0, local_cursor.1);
                break;
            }

            // Cursor doesn't hit linked box, but we're past current box
            // Check if cursor is also past the linked box (it might be in a gap further along)
            let is_past_linked = match direction {
                SelectionDirection::Forward => self.text_boxes[linked_key].is_cursor_past_end(cursor_pos),
                SelectionDirection::Backward => self.text_boxes[linked_key].is_cursor_before_start(cursor_pos),
            };

            if is_past_linked {
                // Add linked box to multi_box_selection since we'll extend it in next iteration
                if !self.shared.multi_box_selection.contains(&linked_key) {
                    self.shared.multi_box_selection.push(linked_key);
                }
                current_key = linked_key;
            } else {
                // Cursor is in the gap but not past linked box
                // Don't extend further
                break;
            }
        }

        did_extend
    }

    fn handle_left_click(&mut self, new_focus: Option<AnyBox>) -> bool {
        let shift = self.input_state.modifiers.state().shift_key();
        let mut handled_shift_click = false;

        if shift {
            if let Some(AnyBox::TextBox(target_key)) = new_focus {
                handled_shift_click = self.handle_shift_click_selection(target_key);
            }
        }

        if ! handled_shift_click {
            // Clear visual selections on all multi-box boxes before refocusing
            if self.shared.multi_box_selection.len() > 1 {
                for &k in &self.shared.multi_box_selection {
                    self.text_boxes[k].selection = parley::Selection::default();
                }
                self.shared.rebuild_glyph_quad_buffer = true;
            }
            self.shared.multi_box_selection.clear();

            self.refocus(new_focus);

            // Set cross-box selection anchor for non-shift clicks on TextBox.
            if let Some(AnyBox::TextBox(key)) = new_focus {
                if self.shared.multi_box_selection.is_empty() {
                    self.shared.multi_box_selection.push(key);
                }

                self.shared.cross_box_selection_anchor = Some(key);
            } else {
                self.shared.cross_box_selection_anchor = None;
            }
            self.shared.cross_box_cursor_key = None;
        }

        handled_shift_click
    }

    /// Handle shift-click selection on TextBoxes.
    /// Uses the stored anchor to create selection spanning from anchor box to target box.
    fn handle_shift_click_selection(&mut self, target_key: DefaultKey) -> bool {
        let Some(anchor_key) = self.shared.cross_box_selection_anchor else {
            return false;
        };

        // Get click position in target's local coords
        let cursor = self.input_state.mouse.cursor_pos;
        let (click_x, click_y) = {
            let tb = &self.text_boxes[target_key];
            let inv = tb.transform().inverse().unwrap_or(Transform2D::identity());
            let p = inv.transform_point(euclid::Point2D::new(cursor.0 as f32, cursor.1 as f32));
            (p.x + tb.scroll_offset.0, p.y + tb.scroll_offset.1)
        };

        // Same box: use shift_click_extension which preserves word/line granularity
        if anchor_key == target_key {
            let tb = &mut self.text_boxes[target_key];
            let new_sel = tb.selection.shift_click_extension(&tb.layout, click_x, click_y);
            tb.selection = new_sel;
            self.shared.cross_box_cursor_key = None;
            self.shared.rebuild_glyph_quad_buffer = true;
            return true;
        }

        // Determine direction by checking reachability without allocating
        let is_forward = {
            let mut cur = anchor_key;
            let mut found = false;
            while let Some(next) = self.text_boxes.get(cur).and_then(|b| b.next_box) {
                if next == target_key { found = true; break; }
                cur = next;
            }
            found
        };

        if !is_forward {
            // Verify target is reachable backward
            let mut cur = anchor_key;
            let mut found = false;
            while let Some(prev) = self.text_boxes.get(cur).and_then(|b| b.prev_box) {
                if prev == target_key { found = true; break; }
                cur = prev;
            }
            if !found {
                return false; // Not linked
            }
        }

        // Clear old selections (except anchor box which we'll extend)
        for &k in &self.shared.multi_box_selection {
            if k != anchor_key {
                self.text_boxes[k].selection = parley::Selection::default();
            }
        }
        self.shared.multi_box_selection.clear();

        // Boundary points
        let boundary_end = if is_forward { (f32::MAX, f32::MAX) } else { (0.0_f32, 0.0_f32) };
        let boundary_start = if is_forward { (0.0_f32, 0.0_f32) } else { (f32::MAX, f32::MAX) };

        // Walk the chain and apply selections directly
        let mut current = anchor_key;
        loop {
            let is_first = current == anchor_key;
            let is_last = current == target_key;

            {
                let tb = &mut self.text_boxes[current];
                if is_first {
                    // Anchor box: extend existing selection to boundary
                    tb.selection.extend_selection_to_point(&tb.layout, boundary_end.0, boundary_end.1);
                } else if is_last {
                    // Target box: place cursor at boundary edge, extend to exact click point (no snapping)
                    tb.selection.move_to_point(&tb.layout, boundary_start.0, boundary_start.1);
                    tb.selection.extend_selection_to_point(&tb.layout, click_x, click_y);
                } else {
                    // Middle box: select all
                    tb.selection.move_to_point(&tb.layout, boundary_start.0, boundary_start.1);
                    tb.selection.extend_selection_to_point(&tb.layout, boundary_end.0, boundary_end.1);
                }
            }
            self.shared.multi_box_selection.push(current);

            if is_last { break; }

            let next = if is_forward {
                self.text_boxes[current].next_box
            } else {
                self.text_boxes[current].prev_box
            };
            match next {
                Some(k) => current = k,
                None => break,
            }
        }

        self.shared.cross_box_cursor_key = Some(target_key);
        self.shared.rebuild_glyph_quad_buffer = true;
        true
    }

    /// Returns true if the event was consumed.
    fn handle_keyboard_selection(
        &mut self,
        focused_key: DefaultKey,
        event: &KeyEvent,
        action_mod: bool,
    ) -> bool {
        let cursor_key = self.shared.cross_box_cursor_key.unwrap_or(focused_key);
        let sel_before = self.text_boxes[cursor_key].selection;

        let is_forward = {
            let tb = &mut self.text_boxes[cursor_key];
            match apply_shift_nav_op(&mut tb.selection, &tb.layout, event, action_mod) {
                Some(fwd) => fwd,
                None => return false,
            }
        };

        if self.text_boxes[cursor_key].selection != sel_before {
            self.shared.rebuild_glyph_quad_buffer = true;
            return true;
        }

        // Cursor didn't move — it's at the text boundary of cursor_key.

        if cursor_key == focused_key {
            // Try to extend into a linked box in the direction of the operation.
            let next_key = if is_forward {
                self.text_boxes[focused_key].next_box
            } else {
                self.text_boxes[focused_key].prev_box
            };
            if let Some(next_key) = next_key {
                let entry = if is_forward { (0.0_f32, 0.0_f32) } else { (f32::MAX, f32::MAX) };
                {
                    let tb = &mut self.text_boxes[next_key];
                    tb.selection.move_to_point(&tb.layout, entry.0, entry.1);
                    apply_shift_nav_op(&mut tb.selection, &tb.layout, event, action_mod);
                }
                self.shared.cross_box_cursor_key = Some(next_key);
                if !self.shared.multi_box_selection.contains(&next_key) {
                    self.shared.multi_box_selection.push(next_key);
                }
                self.shared.rebuild_glyph_quad_buffer = true;
            }
            // else: at absolute chain boundary — event consumed but nothing to do.
            return true;
        }

        // cursor_key != focused_key: in an extended box.
        // Determine extension direction and whether this op retracts or extends further.
        let is_forward_extension = self.is_cursor_forward_from_focused(focused_key, cursor_key);
        let is_retracting = is_forward_extension != is_forward;

        if is_retracting {
            // Find the box toward focused before modifying state.
            let prev_toward_focused = if is_forward_extension {
                self.text_boxes[cursor_key].prev_box
            } else {
                self.text_boxes[cursor_key].next_box
            };

            // Clear cursor box and remove from multi-box selection.
            self.text_boxes[cursor_key].selection = parley::Selection::default();
            self.shared.multi_box_selection.retain(|&k| k != cursor_key);

            let new_cursor = prev_toward_focused.unwrap_or(focused_key);
            self.shared.cross_box_cursor_key = if new_cursor == focused_key { None } else { Some(new_cursor) };

            // Apply op to new cursor box — its cursor was at the far boundary, step inward.
            {
                let tb = &mut self.text_boxes[new_cursor];
                apply_shift_nav_op(&mut tb.selection, &tb.layout, event, action_mod);
            }
        } else {
            // Extend further in the same direction.
            let next_key = if is_forward {
                self.text_boxes[cursor_key].next_box
            } else {
                self.text_boxes[cursor_key].prev_box
            };
            if let Some(next_key) = next_key {
                let entry = if is_forward { (0.0_f32, 0.0_f32) } else { (f32::MAX, f32::MAX) };
                {
                    let tb = &mut self.text_boxes[next_key];
                    tb.selection.move_to_point(&tb.layout, entry.0, entry.1);
                    apply_shift_nav_op(&mut tb.selection, &tb.layout, event, action_mod);
                }
                self.shared.cross_box_cursor_key = Some(next_key);
                if !self.shared.multi_box_selection.contains(&next_key) {
                    self.shared.multi_box_selection.push(next_key);
                }
            }
        }

        self.shared.rebuild_glyph_quad_buffer = true;
        true
    }

    /// Returns true if cursor_key is reachable by following next_box links from focused_key.
    fn is_cursor_forward_from_focused(&self, focused_key: DefaultKey, cursor_key: DefaultKey) -> bool {
        let mut cur = focused_key;
        while let Some(next) = self.text_boxes.get(cur).and_then(|b| b.next_box) {
            if next == cursor_key { return true; }
            cur = next;
        }
        false
    }

    /// Set the disabled state of a text edit box.
    /// 
    /// When disabled, the text edit will not respond to events and will be rendered with greyed out text.
    pub fn set_text_edit_disabled(&mut self, handle: &TextEditHandle, disabled: bool) {
        let text_edit = &mut self.text_edits[handle.key];
        text_edit.disabled = disabled;
        if disabled {
            if let Some(AnyBox::TextEdit(e)) = self.shared.focused {
                if e == handle.key {
                    self.get_text_edit_mut(&handle).text_box.reset_selection();
                    self.shared.focused = None;
                }
            }
        }

    }

    /// Returns `true` if scrolling occurred in the last frame.
    pub fn scrolled(&self) -> bool {
        self.shared.scrolled
    }


    /// Returns `true` if the text content needs to be redrawn.
    /// 
    /// This function is useful to decide whether to call `winit`'s `Window::request_redraw()` after processing a `winit` event.
    /// 
    /// Games and applications that rerender continuously can call `Window::request_redraw()` unconditionally after every `RedrawRequested` event, without checking this method.
    pub fn needs_rerender(&mut self) -> bool {
        return self.shared.rebuild_glyph_quad_buffer || self.shared.rerender_cursor || self.shared.scrolled || !self.scrolled_moved_indices.is_empty();
    }

    /// Get a mutable reference to a text box wrapped with its style.
    /// 
    /// `handle` is the handle that was returned when first creating the text box with [`Text::add_text_box()`].
    /// 
    /// This is a fast lookup operation that does not require any hashing.
    pub fn get_text_box_mut(&mut self, handle: &TextBoxHandle) -> &mut TextBox {
        return &mut self.text_boxes[handle.key];
    }


    /// Get a mutable reference to a text box wrapped with its style.
    /// 
    /// `handle` is the handle that was returned when first creating the text box with [`Text::add_text_box()`].
    /// 
    /// This is a fast lookup operation that does not require any hashing.
    pub fn get_text_box(&self, handle: &TextBoxHandle) -> &TextBox {
        return &self.text_boxes[handle.key];
    }

    /// Returns a text box if it exists, or `None` if it has been removed.
    pub fn try_get_text_box(&self, handle: &ClonedTextBoxHandle) -> Option<&TextBox> {
        return self.text_boxes.get(handle.key);
    }

    /// Returns a mutable text box if it exists, or `None` if it has been removed.
    pub fn try_get_text_box_mut(&mut self, handle: &ClonedTextBoxHandle) -> Option<&mut TextBox> {
        return self.text_boxes.get_mut(handle.key);
    }

    /// Link two text boxes for cross-box selection.
    ///
    /// When selecting past the end of `first`, the selection will continue into `second`.
    /// When selecting before the start of `second`, the selection will continue into `first`.
    /// This only affects non-editable text boxes (TextBox, not TextEdit).
    pub fn link_text_boxes(&mut self, first: &TextBoxHandle, second: &TextBoxHandle) {
        self.text_boxes[first.key].next_box = Some(second.key);
        self.text_boxes[second.key].prev_box = Some(first.key);
    }

    /// Remove all cross-box selection links involving this text box.
    ///
    /// Clears `prev_box` and `next_box` on this box, and also clears the
    /// corresponding back-pointer on each former neighbor.
    pub fn unlink_text_box(&mut self, handle: &TextBoxHandle) {
        let (prev_key, next_key) = match self.text_boxes.get(handle.key) {
            Some(tb) => (tb.prev_box, tb.next_box),
            None => return,
        };
        if let Some(prev_key) = prev_key {
            if let Some(prev_tb) = self.text_boxes.get_mut(prev_key) {
                prev_tb.next_box = None;
            }
        }
        if let Some(next_key) = next_key {
            if let Some(next_tb) = self.text_boxes.get_mut(next_key) {
                next_tb.prev_box = None;
            }
        }
        let tb = &mut self.text_boxes[handle.key];
        tb.prev_box = None;
        tb.next_box = None;
    }

    /// Returns an iterator over selected text from all text boxes in the current multi-box selection.
    pub fn selected_text_iter(&self) -> impl Iterator<Item = &str> {
        self.shared.multi_box_selection.iter().filter_map(|&key| {
            self.text_boxes.get(key).and_then(|tb| tb.selected_text())
        })
    }

    /// Convenience function that returns the selected text from all text boxes in the current cross-box selection as a single contiguous string, inserting a space between each segment, or `None` if nothing is selected.
    ///
    /// If only one box is selected, a reference to the selected text is returned directly without any copying.
    /// Otherwise, the text is copied into an internal buffer.
    /// 
    /// Use [`Text::selected_text_iter()`] to get a zero-cost iterator over the different segments.
    pub fn selected_text(&mut self) -> Option<&str> {
        if self.shared.multi_box_selection.len() == 1 {
            let key = self.shared.multi_box_selection[0];
            return self.text_boxes.get(key).and_then(|tb| tb.selected_text());
        }

        self.selected_text_buffer.clear();
        for &key in &self.shared.multi_box_selection {
            if let Some(tb) = self.text_boxes.get(key) {
                if let Some(text) = tb.selected_text() {
                    if !self.selected_text_buffer.is_empty() && !self.selected_text_buffer.ends_with(' ') {
                        self.selected_text_buffer.push(' ');
                    }
                    self.selected_text_buffer.push_str(text);
                }
            }
        }

        if self.selected_text_buffer.is_empty() {
            None
        } else {
            Some(&self.selected_text_buffer)
        }
    }

    /// Add a scroll animation for a text edit
    pub(crate) fn add_scroll_animation(&mut self, handle: &TextEditHandle, start_offset: f32, target_offset: f32, duration: std::time::Duration, direction: ScrollDirection) {
        // Remove any existing animation for this handle and direction
        self.scroll_animations.retain(|anim| !(anim.handle.key == handle.key && anim.direction == direction));
        self.shared.scrolled = true;
        
        let animation = ScrollAnimation {
            start_offset,
            target_offset,
            start_time: std::time::Instant::now(),
            duration,
            direction,
            handle: handle.to_cloned(),
        };
        
        self.scroll_animations.push(animation);
    }

    /// Get the maximum remaining animation duration, if any animations are running.
    fn get_max_animation_duration(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut max_remaining = Duration::ZERO;
        let mut has_animations = false;
        
        for animation in &self.scroll_animations {
            let elapsed = now.duration_since(animation.start_time);
            if elapsed < animation.duration {
                let remaining = animation.duration - elapsed;
                if remaining > max_remaining {
                    max_remaining = remaining;
                }
                has_animations = true;
            }
        }
        
        if has_animations {
            Some(max_remaining)
        } else {
            None
        }
    }

    /// Update smooth scrolling animations for all text edits automatically.
    /// Returns true if any text edit animations were updated and require redrawing.
    fn update_smooth_scrolling(&mut self) -> bool {
        let mut needs_redraw = false;
        
        // Update all active animations
        let mut i = 0;
        while i < self.scroll_animations.len() {
            let animation = &self.scroll_animations[i];
            if let Some(text_edit) = self.text_edits.get_mut(animation.handle.key) {
                let current_offset = animation.get_current_offset();
                
                match animation.direction {
                    ScrollDirection::Horizontal => {
                        text_edit.text_box.scroll_offset.0 = current_offset;
                    }
                    ScrollDirection::Vertical => {
                        text_edit.text_box.scroll_offset.1 = current_offset;
                    }
                }
                
                if animation.is_finished() {
                    self.scroll_animations.remove(i);
                    // Don't increment i since we removed an element
                } else {
                    i += 1;
                }
                
                needs_redraw = true;
            } else {
                // Text edit doesn't exist anymore, remove the animation
                self.scroll_animations.remove(i);
            }
        }
        
        needs_redraw
    }

    fn handle_text_edit_scroll_event(&mut self, handle: &TextEditHandle, event: &WindowEvent, _window: &Window) -> bool {
        let mut did_scroll = false;

        if let WindowEvent::MouseWheel { delta, .. } = event {
            let shift_held = self.input_state.modifiers.state().shift_key();
            
            if let Some(te) = self.text_edits.get_mut(handle.key) {
                if te.single_line {
                    // Single-line horizontal scrolling
                    let scroll_amount = match delta {
                        winit::event::MouseScrollDelta::LineDelta(x, y) => {
                            if shift_held {
                                y * 120.0
                            } else {
                                x * 120.0
                            }
                        },
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            if shift_held {
                                pos.y as f32 
                            } else {
                                pos.x as f32
                            }
                        },
                    };
                    
                    if scroll_amount != 0.0 {
                        let current_scroll = te.text_box.scroll_offset.0;
                        let target_scroll = current_scroll - scroll_amount;
                        
                        let total_text_width = te.text_box.layout.full_width();
                        let text_width = te.text_box.max_advance;
                        let max_scroll = (total_text_width - text_width).max(0.0).round() + crate::text_edit::CURSOR_WIDTH;
                        let clamped_target = target_scroll.clamp(0.0, max_scroll).round();
                        
                        if (clamped_target - current_scroll).abs() > 0.1 {
                            if should_use_animation(delta, shift_held) {
                                let animation_duration = std::time::Duration::from_millis(200);
                                self.add_scroll_animation(handle, current_scroll, clamped_target, animation_duration, ScrollDirection::Horizontal);
                            } else {
                                te.text_box.scroll_offset.0 = clamped_target;
                            }
                            did_scroll = true;
                        }
                    }
                } else {
                    // Multi-line vertical scrolling
                    let scroll_amount = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_x, y) => y * 120.0,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y as f32,
                    };
                    
                    if scroll_amount != 0.0 {
                        let current_scroll = te.text_box.scroll_offset.1;
                        let target_scroll = current_scroll - scroll_amount;
                        
                        let total_text_height = te.text_box.layout.height();
                        let text_height = te.text_box.height;
                        let max_scroll = (total_text_height - text_height).max(0.0).round();
                        let clamped_target = target_scroll.clamp(0.0, max_scroll).round();
                        
                        if (clamped_target - current_scroll).abs() > 0.1 {
                            if should_use_animation(delta, true) {
                                let animation_duration = std::time::Duration::from_millis(200);
                                self.add_scroll_animation(handle, current_scroll, clamped_target, animation_duration, ScrollDirection::Vertical);
                            } else {
                                te.text_box.scroll_offset.1 = clamped_target;
                            }
                            did_scroll = true;
                        }
                    }
                }
            }
        }

        did_scroll
    }

    /// Returns the duration until the next cursor blink state change.
    ///
    /// Returns `None` if cursor blinking should not be blinking.
    pub fn time_until_next_cursor_blink(&self) -> Option<Duration> {
        if let Some(start_time) = self.shared.cursor_blink_start {
            let elapsed = Instant::now().duration_since(start_time);
            let blink_period = Duration::from_millis(CURSOR_BLINK_TIME_MILLIS);
            let elapsed_in_current_cycle = elapsed.as_millis() % blink_period.as_millis();
            let time_until_next_blink = blink_period.as_millis() - elapsed_in_current_cycle;
            Some(Duration::from_millis(time_until_next_blink as u64))
        } else {
            None
        }
    }

    // todo: would be a lot nicer to have these as methods on TextBoxMut and TextEditMut, but is it worth carrying the key around just for this?
    /// Sets focus to the specified text box.
    pub fn set_focus_to_text_box(&mut self, handle: &TextBoxHandle) {
        let handle: AnyBox = (*handle).get_anybox();
        self.refocus(Some(handle));
    }
    /// Sets focus to the specified text edit.
    pub fn set_focus_to_text_edit(&mut self, handle: &TextEditHandle) {
        let handle: AnyBox = (*handle).get_anybox();
        self.refocus(Some(handle));
    }

    #[cfg(feature = "accessibility")]
    /// Sets the accessibility ID for a text box.
    pub fn set_text_box_accesskit_id(&mut self, handle: &TextBoxHandle, accesskit_id: NodeId) {
        let any_box = handle.keynto_anybox();
        self.accesskit_id_to_text_handle_map.insert(accesskit_id, any_box);
        self.get_text_box_mut(handle).set_accesskit_id(accesskit_id);
    }
    
    #[cfg(feature = "accessibility")]
    /// Sets the accessibility ID for a text edit.
    pub fn set_text_edit_accesskit_id(&mut self, handle: &TextEditHandle, accesskit_id: NodeId) {
        let any_box = handle.keynto_anybox();
        self.accesskit_id_to_text_handle_map.insert(accesskit_id, any_box);
        self.get_text_edit_mut(handle).set_accesskit_id(accesskit_id);
    }
    
    /// Get the text handle for a given AccessKit node ID
    #[cfg(feature = "accessibility")]
    pub(crate) fn get_text_handle_by_accesskit_id(&self, node_id: NodeId) -> Option<AnyBox> {
        self.accesskit_id_to_text_handle_map.get(&node_id).copied()
    }

    #[cfg(feature = "accessibility")]
    /// Sets focus by accessibility node ID.
    pub fn set_focus_by_accesskit_id(&mut self, focus: NodeId) {
        if let Some(focused_text_handle) = self.get_text_handle_by_accesskit_id(focus) {
            self.set_focus(&focused_text_handle);
        }
    }
    
    /// Set a custom node ID generator function for accessibility
    /// 
    /// The generator function will be called whenever a new accessibility node ID is needed.
    /// This allows you to control the node ID allocation strategy.
    /// 
    /// # Example
    /// ```ignore
    /// use accesskit::NodeId;
    /// 
    /// fn my_generator() -> NodeId {
    ///     // Your custom logic here
    ///     NodeId(42)
    /// }
    /// 
    /// text.set_node_id_generator(my_generator);
    /// ```
    #[cfg(feature = "accessibility")]
    pub fn set_node_id_generator(&mut self, generator: fn() -> NodeId) {
        self.shared.node_id_generator = generator;
    }

    /// Returns the currently focused text widget, if any.
    pub fn focus(&self) -> Option<AnyBox> {
        self.shared.focused
    }

    /// Returns a mutable reference to the FontContext.
    pub fn font_context(&mut self) -> &mut FontContext {
        &mut self.shared.font_cx
    }

    /// Returns a mutable reference to the LayoutContext.
    pub fn layout_context(&mut self) -> &mut LayoutContext<ColorBrush> {
        &mut self.shared.layout_cx
    }

    /// Helper method to load a font from font data and return the family name which can be used in to refer to it in a text style.
    /// 
    /// Returns `None` if the font data is invalid or contains no fonts.
    /// 
    /// For more advanced use cases, use [`Text::font_context()`] to get a mutable reference to the parley `FontContext`. The `collection` field of the `FontContext` is an instance of a `fontique` `Collection`, which offers lower level control.
    /// 
    /// # Example
    /// ```ignored
    /// # use keru_text::*;
    /// # use parley::FontFamily;
    /// # let text = Text::new();
    /// let family_name = text.load_font(include_bytes!("../MyFont.ttf"))
    ///     .expect("Failed to load font");
    /// let style = text.add_style(TextStyle {
    ///     font_stack: FontStack::Single(FontFamily::Named(family_name.into())),
    ///     ..Default::default()
    /// }, None);
    /// 
    /// # let text_box: TextBoxHandle = unimplemented!();
    /// text.get_text_box_mut(&text_box).set_style(&style);
    /// ```
    pub fn load_font(&mut self, font_data: &[u8]) -> Option<String> {
        let families = self.shared.font_cx.collection.register_fonts(font_data.to_vec().into(), None);
        let family_id = families.first()?.0;
        let family = self.shared.font_cx.collection.family(family_id)?;
        Some(family.name().to_string())
    }

    /// Set an inserted style as the default style.
    pub fn set_default_style(&mut self, style: &StyleHandle) {
        self.shared.default_style_key = style.key;
        // todo set needs relayout?
    }

    #[cfg(feature = "accessibility")]
    /// Returns the accessibility node ID of the currently focused text element.
    pub fn focused_accesskit_id(&self) -> Option<NodeId> {
        if let Some(focused) = self.shared.focused {
            match focused {
                AnyBox::TextEdit(i) => {
                    if let Some((_text_edit, text_box)) = self.text_edits.get(i) {
                        text_box.accesskit_id
                    } else {
                        None
                    }
                }
                AnyBox::TextBox(i) => {
                    if let Some(text_box) = self.text_boxes.get(i as usize) {
                        text_box.accesskit_id
                    } else {
                        None
                    }
                }
            }
        } else {
            None
        }
    }

    /// Returns an Accesskit update for all the changes to the text content that happened since the last update, or `None` if nothing happened at all.
    /// 
    /// It can be very hard to understand what actually happened to the focus from the data in the `accesskit::TreeUpdate` alone, so this function also returns a [`FocusUpdate`]. It might be wiser for users of this function to read the value of `FocusUpdate` and fill in the value of `TreeUpdate`'s `focus` themselves.
    /// 
    /// In particular, if the user clicks outside of all text boxes, the `TreeUpdate`'s `focus` will be set to `root_node_id`, because that's what Accesskit wants to signal that nothing is focused anymore. But this means that if the focus went to some other non-text element in the GUI library, the GUI library will have to send its update *after* this one, or it will be overwritten by the `root_node_id`. 
    /// 
    /// Ideally, Accesskit would allow `Text` to report that a `NodeId` just lost focus, and figure out itself what to do from there. (Actually, it would probably be a list of nodes that definitely don't have focus anymore. I guess that would be a bit complicated.)
    #[cfg(feature = "accessibility")]
    pub fn accesskit_update(&mut self, current_focused_node_id: Option<NodeId>, root_node_id: NodeId) -> Option<(TreeUpdate, FocusUpdate)> {
        // For some reason, every update that we send must specify the focus again.
        // If something else changed it, we'd end up overriding it.
        // So we have to ask for the current one from outside and fill that in, in case that nothing happened.
        // According to the TreeUpdate docs, we should also set focus = root_node_id when text boxes are defocused.
        // However, this means that the focus actually goes to the whole window, Windows Narrator says the name of the window, and the blue box covers the whole window. I don't really see this behavior anywhere else.

        let mut focus_update = FocusUpdate::Unchanged;

        let old_focus = self.shared.accesskit_focus_tracker.old_focus;
        let new_focus = self.shared.accesskit_focus_tracker.new_focus;

        if old_focus != new_focus {
            focus_update = FocusUpdate::Changed { old_focus, new_focus };
        }
        
        // Make a different focus update to try to figure out the least wrong thing to stick in the TreeUpdate.
        let mut focus_update_for_tree = focus_update;

        // If the focus update is old, it might be riskier to even report it. Not sure, though.
        let focus_update_is_fresh = self.shared.current_event_number == self.shared.accesskit_focus_tracker.event_number;
        if ! focus_update_is_fresh {
            focus_update_for_tree = FocusUpdate::Unchanged;
        }

        if (focus_update_for_tree == FocusUpdate::Unchanged) && self.shared.accesskit_tree_update.nodes.is_empty() {
            return None;
        }

        let current_focused_node_id = current_focused_node_id.unwrap_or(root_node_id);

        let focus_value_for_tree = match focus_update_for_tree {
            FocusUpdate::Changed { old_focus: _, new_focus } => {
                if let Some(new_focus) = new_focus {
                    new_focus
                } else {
                    root_node_id
                }
            }
            FocusUpdate::Unchanged => current_focused_node_id,
        };

        self.shared.accesskit_tree_update.focus = focus_value_for_tree;
        let res = self.shared.accesskit_tree_update.clone();

        // Reset to an empty update.
        self.shared.accesskit_tree_update.nodes.clear();
        self.shared.accesskit_tree_update.tree = None;

        return Some((res, focus_update));
    }

    #[cfg(feature = "accessibility")]
    fn push_ak_update_for_focused(&mut self, focused: AnyBox) {
        match focused {
            AnyBox::TextEdit(i) => {
                let handle = TextEditHandle { i };
                let mut text_edit = self.get_text_edit_mut(&handle);
                text_edit.push_accesskit_update_to_self();
            },
            AnyBox::TextBox(i) => {
                let handle = TextBoxHandle { i };
                let mut text_box = self.get_text_box_mut(&handle);
                text_box.push_accesskit_update_to_self();
            },
        }
    }
}

/// Update scroll by adjusting BoxGpu translation instead of modifying quad positions.
/// Returns false if scroll has exceeded the tolerance from the base position (line culling boundary),
/// in which case a full re-prepare is needed to get the correct lines.
fn update_scroll(render_data: &mut RenderData, quad_storage: &mut RenderDataInfo, current_scroll: (f32, f32)) -> bool {
    // Check if we've scrolled too far from the base (line culling tolerance)
    // Compute delta from last scroll position
    let delta_x = current_scroll.0 - quad_storage.last_scroll.0;
    let delta_y = current_scroll.1 - quad_storage.last_scroll.1;

    // Adjust BoxGpu translation and clip_rect for scroll
    render_data.adjust_box_for_scroll(quad_storage.box_index, delta_x, delta_y);

    // Update last_scroll to track the current state
    quad_storage.last_scroll = current_scroll;
    true
}

// todo: get this from system settings.
const CURSOR_BLINK_TIME_MILLIS: u64 = 500;

#[derive(Debug)]
enum WakerCommand {
    Start,
    Stop,
    Exit,
}

pub(crate) struct CursorBlinkWaker {
    command_sender: mpsc::Sender<WakerCommand>,
}

impl Drop for CursorBlinkWaker {
    fn drop(&mut self) {
        // Signal the thread to exit
        let _ = self.command_sender.send(WakerCommand::Exit);
    }
}

impl CursorBlinkWaker {
    fn new(window: Weak<Window>) -> Self {
        let (command_sender, command_receiver) = mpsc::channel();
        
        thread::spawn(move || {
            let mut is_running = false;
            
            loop {
                if is_running {
                    // While running, wait for either a command or timeout
                    match command_receiver.recv_timeout(Duration::from_millis(CURSOR_BLINK_TIME_MILLIS)) {
                        Ok(WakerCommand::Start) => {}
                        Ok(WakerCommand::Stop) => is_running = false,
                        Ok(WakerCommand::Exit) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // Timeout occurred, request redraw directly
                            if let Some(window) = window.upgrade() {
                                window.request_redraw();
                            } else {
                                // Window has been dropped, exit thread
                                return;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    // While stopped, wait indefinitely for a command
                    match command_receiver.recv() {
                        Ok(WakerCommand::Start) => is_running = true,
                        Ok(WakerCommand::Stop) => {}
                        Ok(WakerCommand::Exit) => return,
                        Err(_) => return,
                    }
                }
            }
        });
        
        Self {
            command_sender,
        }
    }
        
    fn start(&self) {
        let _ = self.command_sender.send(WakerCommand::Start);
    }
    
    fn stop(&self) {
        let _ = self.command_sender.send(WakerCommand::Stop);
    }
}
