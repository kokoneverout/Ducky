//! Reproducible, transparent builtin animations. Run only when changing assets.
use image::{
    Delay, Frame, Rgba, RgbaImage,
    codecs::gif::{GifEncoder, Repeat},
};
fn main() -> anyhow::Result<()> {
    for (name, processing) in [("recording", false), ("processing", true)] {
        let mut encoder = GifEncoder::new(std::fs::File::create(format!("assets/{name}.gif"))?);
        encoder.set_repeat(Repeat::Infinite)?;
        for step in 0..24 {
            let mut frame = RgbaImage::new(168, 52);
            let phase = step as f32 / 24.0 * std::f32::consts::TAU;
            for bar in 0..19 {
                let envelope = (1.0 - ((bar as f32 - 9.0) / 10.0).abs()).max(0.15);
                let movement = (phase + bar as f32 * 0.55).sin() * 0.5 + 0.5;
                let height = if processing {
                    4.0 + 18.0 * movement.powi(3)
                } else {
                    5.0 + 28.0 * envelope * (0.2 + 0.8 * movement)
                };
                let color = if processing {
                    [122, 142, 103, 255]
                } else if bar < 7 {
                    [101, 143, 136, 255]
                } else if bar < 13 {
                    [126, 151, 104, 255]
                } else {
                    [168, 153, 105, 255]
                };
                let x = 9 + bar * 8;
                let top = (26.0 - height / 2.0) as u32;
                let bottom = (26.0 + height / 2.0) as u32;
                for y in top..bottom {
                    for dx in 0..4 {
                        frame.put_pixel(x + dx, y, Rgba(color));
                    }
                }
            }
            encoder.encode_frame(Frame::from_parts(
                frame,
                0,
                0,
                Delay::from_numer_denom_ms(50, 1),
            ))?;
        }
    }
    Ok(())
}
