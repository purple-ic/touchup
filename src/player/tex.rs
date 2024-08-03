use std::sync::{Arc, Mutex, TryLockError};

use eframe::glow::{HasContext, PixelUnpackData};
use eframe::{
    egui_glow::check_for_gl_error,
    glow::{self, TEXTURE_2D},
    Frame,
};
use egui::{Context, TextureId};
use log::{debug, info};

pub type TextureArc = Arc<Mutex<PlayerTexture>>;

pub struct PlayerTexture {
    /// \[width, height] in pixels
    pub size: [usize; 2],
    /// in rgb24 bytes
    pub bytes: Vec<u8>,
    pub changed: bool,
}

pub struct CurrentTex {
    pub id: TextureId,
    pub native: glow::Texture,
    pub size: [usize; 2],
}

fn create_texture(size: [usize; 2], data: &[u8], frame: &mut Frame) -> CurrentTex {
    let gl = frame.gl().unwrap();
    let texture = unsafe { gl.create_texture() }.unwrap();
    init_texture(texture, gl, size, data);
    check_for_gl_error!(gl, "trying to initialize texture");
    let texture_id = frame.register_native_glow_texture(texture);
    CurrentTex {
        id: texture_id,
        native: texture,
        size,
    }
}

fn init_texture(texture: glow::Texture, gl: &glow::Context, size: [usize; 2], data: &[u8]) {
    unsafe {
        gl.get_error();
        gl.bind_texture(TEXTURE_2D, Some(texture));
        set_player_texture_settings(gl);
        // using RGB instead of SRGB makes the images.. way brighter?
        //  kind of interesting that it makes them brighter instead of loosing quality
        //  even though the source format is still the same
        gl.tex_image_2d(
            TEXTURE_2D,
            0,
            glow::SRGB8 as i32,
            size[0]
                .try_into()
                .expect("could not convert size from usize to i32"),
            size[1]
                .try_into()
                .expect("could not convert size from usize to i32"),
            0,
            glow::RGB,
            glow::UNSIGNED_BYTE,
            Some(data),
        );
        gl.bind_texture(TEXTURE_2D, None);
    }
}

pub fn attempt_tex_update(
    current_tex: &mut Option<CurrentTex>,
    texture: &Mutex<PlayerTexture>,
    ctx: &Context,
    frame: &mut Frame,
) {
    let mut tex = match texture.try_lock() {
        Ok(v) => v,
        Err(TryLockError::WouldBlock) => return,
        Err(TryLockError::Poisoned(p)) => {
            debug!("clearing poison from player texture mutex");
            texture.clear_poison();
            p.into_inner()
        }
    };

    // todo: this texture system leaves a frame or two where the last-played-frame of the previous video is visible
    if tex.changed {
        tex.changed = false;

        if let Some(current_tex) = current_tex {
            if current_tex.size != tex.size {
                debug!("player texture resizing");

                current_tex.size = tex.size;
                let gl = frame.gl().unwrap();
                // tell gl to reallocate the texture with a new size
                init_texture(current_tex.native, gl, tex.size, &tex.bytes);
                check_for_gl_error!(gl, "trying to reinitialize texture for new size");
            } else {
                // println!("updating texture");
                let gl = frame.gl().unwrap();
                unsafe {
                    gl.get_error();
                    gl.bind_texture(TEXTURE_2D, Some(current_tex.native));
                    set_player_texture_settings(gl);
                    gl.tex_sub_image_2d(
                        TEXTURE_2D,
                        0,
                        0,
                        0,
                        tex.size[0]
                            .try_into()
                            .expect("could not convert size from usize to i32"),
                        tex.size[1]
                            .try_into()
                            .expect("could not convert size from usize to i32"),
                        glow::RGB,
                        glow::UNSIGNED_BYTE,
                        PixelUnpackData::Slice(&tex.bytes),
                    );
                    gl.bind_texture(TEXTURE_2D, None);
                    check_for_gl_error!(&gl, "while attempting to update the player texture")
                }
            }
        } else {
            info!("creating player texture");
            *current_tex = Some(create_texture(tex.size, &tex.bytes, frame));
        };
    }
}

fn set_player_texture_settings(gl: &glow::Context) {
    unsafe {
        gl.tex_parameter_i32(TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);

        gl.tex_parameter_i32(TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    }
    check_for_gl_error!(gl, "trying to set tex params");
}
