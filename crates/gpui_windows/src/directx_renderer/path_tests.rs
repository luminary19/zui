//! Compare region scratch to the original full-window rasterization and sampling.
use super::{
    backdrop_tests::{TestWindow, bounds, pixels, quad, warp_renderer},
    *,
};
use windows::{
    Win32::Graphics::Direct3D::{
        Fxc::{
            D3DCOMPILE_DEBUG, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCOMPILE_SKIP_OPTIMIZATION,
            D3DCompileFromFile,
        },
        ID3DBlob,
    },
    core::{PCSTR, s},
};

fn full_window_shader(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let shader_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/directx_renderer/path_tests.hlsl");
    let mut blob = None;
    let mut errors = None;
    let flags = if cfg!(debug_assertions) {
        D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION
    } else {
        D3DCOMPILE_OPTIMIZATION_LEVEL3
    };
    unsafe {
        let result = D3DCompileFromFile(
            &HSTRING::from(shader_path.to_str().unwrap()),
            None,
            None,
            entry,
            target,
            flags,
            0,
            &mut blob,
            Some(&mut errors),
        );
        if let Some(errors) = errors {
            result.with_context(|| {
                std::ffi::CStr::from_ptr(errors.GetBufferPointer().cast())
                    .to_string_lossy()
                    .into_owned()
            })?;
        } else {
            result?;
        }
    }
    Ok(blob.unwrap())
}

fn full_window_renderer() -> Result<(DirectXRenderer, TestWindow)> {
    let (mut renderer, window) = warp_renderer()?;
    let device = &renderer.devices.as_ref().unwrap().device;
    let vertex = full_window_shader(s!("path_sprite_vertex"), s!("vs_4_1"))?;
    let fragment = full_window_shader(s!("path_sprite_fragment"), s!("ps_4_1"))?;
    unsafe {
        renderer.pipelines.path_sprite_pipeline.vertex = create_vertex_shader(
            device,
            slice::from_raw_parts(vertex.GetBufferPointer().cast(), vertex.GetBufferSize()),
        )?;
        renderer.pipelines.path_sprite_pipeline.fragment = create_fragment_shader(
            device,
            slice::from_raw_parts(fragment.GetBufferPointer().cast(), fragment.GetBufferSize()),
        )?;
    }
    Ok((renderer, window))
}

fn full_window_batch(renderer: &mut DirectXRenderer, paths: &[Path<ScaledPixels>]) -> Result<()> {
    let devices = renderer.devices.as_ref().unwrap();
    let resources = renderer.resources.as_ref().unwrap();
    let scratch = resources.path_scratch.as_ref().unwrap();
    let mut vertices = Vec::new();
    for path in paths {
        vertices.extend(path.vertices.iter().map(|vertex| PathRasterizationSprite {
            xy_position: vertex.xy_position,
            st_position: vertex.st_position,
            color: path.color,
            bounds: path.clipped_bounds(),
        }));
    }
    renderer.pipelines.path_rasterization_pipeline.update_buffer(
        &devices.device,
        &devices.device_context,
        &vertices,
    )?;
    unsafe {
        devices
            .device_context
            .ClearRenderTargetView(scratch.msaa_view.as_ref().unwrap(), &[0.; 4]);
        devices
            .device_context
            .OMSetRenderTargets(Some(slice::from_ref(&scratch.msaa_view)), None);
    }
    // Zero origin and the unmodified window viewport reproduce the old raster pass.
    renderer.pipelines.path_rasterization_pipeline.draw(
        &devices.device_context,
        slice::from_ref(&resources.viewport),
        &[
            renderer.globals.global_params_buffer.clone(),
            None,
            renderer.globals.path_origin_buffer.clone(),
        ],
        D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        vertices.len() as u32,
        1,
    )?;
    unsafe {
        devices.device_context.ResolveSubresource(
            &scratch.texture,
            0,
            &scratch.msaa_texture,
            0,
            RENDER_TARGET_FORMAT,
        );
        devices.device_context.OMSetRenderTargets(
            Some(slice::from_ref(&resources.render_target_view)),
            None,
        );
    }
    renderer.draw_paths_from_intermediate(paths)
}

fn full_window_pixels(
    renderer: &mut DirectXRenderer,
    scene: &Scene,
    appearance: WindowBackgroundAppearance,
) -> Result<Vec<[u8; 4]>> {
    renderer.pre_draw(&match appearance {
        WindowBackgroundAppearance::Opaque => [1.; 4],
        _ => [0.; 4],
    })?;
    renderer.upload_scene_buffers(scene)?;
    let resources = renderer.resources.as_mut().unwrap();
    if !resources.path_scratch.as_ref().is_some_and(|scratch| {
        scratch.cap_w == renderer.width && scratch.cap_h == renderer.height
    }) {
        resources.path_scratch = None;
        resources.path_scratch = Some(PathScratch::new(
            &renderer.devices.as_ref().unwrap().device,
            renderer.width,
            renderer.height,
        )?);
    }
    update_buffer(
        &renderer.devices.as_ref().unwrap().device_context,
        renderer.globals.path_origin_buffer.as_ref().unwrap(),
        &[PathOrigin {
            origin: [0.; 2],
            tex_size: [renderer.width as f32, renderer.height as f32],
        }],
    )?;
    for batch in scene.batches() {
        match batch {
            PrimitiveBatch::Quads(range) => renderer.draw_quads(range.start, range.len())?,
            PrimitiveBatch::Paths(range) => full_window_batch(renderer, &scene.paths[range])?,
            _ => unreachable!("path regression scenes contain only quads and paths"),
        }
    }
    pixels(renderer)
}

fn test_path(
    order: u32,
    rect: Bounds<ScaledPixels>,
    clip: Bounds<ScaledPixels>,
    color: Background,
) -> Path<ScaledPixels> {
    let x = rect.origin.x.0;
    let y = rect.origin.y.0;
    let w = rect.size.width.0;
    let h = rect.size.height.0;
    let mut path = gpui::Path::new(point(px(x), px(y)));
    path.line_to(point(px(x + w), px(y)));
    path.curve_to(
        point(px(x + w), px(y + h)),
        point(px(x + w * 0.35), px(y + h * 0.8)),
    );
    path.line_to(point(px(x), px(y + h)));
    path.line_to(point(px(x), px(y)));
    let mut path = path.scale(1.);
    path.order = order;
    path.color = color;
    path.content_mask.bounds = clip;
    for vertex in &mut path.vertices {
        vertex.content_mask = path.content_mask;
    }
    path
}

fn test_scene(paths: Vec<Path<ScaledPixels>>, viewport: Bounds<ScaledPixels>) -> Scene {
    let mut scene = Scene::default();
    let mut background = quad(0, viewport, hsla(0.1, 0.3, 0.2, 0.6));
    background.content_mask.bounds = viewport;
    // A later quad catches accidental viewport changes in subsequent pipelines.
    let mut foreground = quad(
        100,
        bounds(3.25, 5.5, 9.5, 11.25),
        hsla(0.7, 0.8, 0.5, 0.4),
    );
    foreground.content_mask.bounds = viewport;
    scene.quads = vec![background, foreground];
    scene.paths = paths;
    scene.finish();
    scene
}

fn assert_same_pixels(actual: &[[u8; 4]], expected: &[[u8; 4]], width: u32, case: &str) {
    assert_eq!(actual.len(), expected.len());
    if let Some(index) = actual.iter().zip(expected).position(|(a, b)| a != b) {
        assert_eq!(
            actual[index],
            expected[index],
            "{case}: pixel ({}, {})",
            index % width as usize,
            index / width as usize,
        );
    }
}

fn scratch(renderer: &DirectXRenderer) -> &PathScratch {
    renderer
        .resources
        .as_ref()
        .unwrap()
        .path_scratch
        .as_ref()
        .unwrap()
}

fn assert_capacity(renderer: &DirectXRenderer, capacity: (u32, u32)) {
    let scratch = scratch(renderer);
    assert_eq!((scratch.cap_w, scratch.cap_h), capacity);
    for texture in [&scratch.texture, &scratch.msaa_texture] {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        assert_eq!((desc.Width, desc.Height), capacity);
    }
}

#[::core::prelude::v1::test]
fn warp_region_paths_match_full_window() -> Result<()> {
    let (mut region, _region_window) = warp_renderer()?;
    let (mut full, _full_window) = full_window_renderer()?;
    let gradient = linear_gradient(
        137.,
        linear_color_stop(hsla(0.05, 0.9, 0.4, 0.35), 0.),
        linear_color_stop(hsla(0.6, 0.8, 0.7, 0.8), 1.),
    );
    for (width, height) in [(320, 256), (321, 259)] {
        region.resize(size(DevicePixels(width), DevicePixels(height)))?;
        full.resize(size(DevicePixels(width), DevicePixels(height)))?;
        let viewport = bounds(0., 0., width as f32, height as f32);
        let cases = vec![
            (
                "gradient",
                vec![test_path(1, bounds(42.25, 68.5, 74.5, 58.25), viewport, gradient)],
            ),
            (
                "content mask",
                vec![test_path(
                    1,
                    bounds(18.25, 23.5, 150.75, 121.25),
                    bounds(43.5, 48.25, 68.75, 51.5),
                    gradient,
                )],
            ),
            (
                "mixed transparent orders",
                vec![
                    test_path(1, bounds(23.25, 31.5, 99.5, 86.25), viewport, gradient),
                    test_path(
                        2,
                        bounds(61.5, 44.25, 98.25, 95.5),
                        viewport,
                        hsla(0.2, 0.8, 0.5, 0.45).into(),
                    ),
                ],
            ),
            (
                "left edge",
                vec![test_path(1, bounds(-8.25, 45.5, 53.5, 67.25), viewport, gradient)],
            ),
            (
                "top edge",
                vec![test_path(1, bounds(73.25, -8.5, 63.5, 51.25), viewport, gradient)],
            ),
            (
                "right edge",
                vec![test_path(
                    1,
                    bounds(width as f32 - 43.25, 47.5, 53.5, 67.25),
                    viewport,
                    gradient,
                )],
            ),
            (
                "bottom edge",
                vec![test_path(
                    1,
                    bounds(73.25, height as f32 - 43.5, 63.5, 51.25),
                    viewport,
                    gradient,
                )],
            ),
            (
                "disjoint same order",
                vec![
                    test_path(1, bounds(27.25, 39.5, 31.5, 35.25), viewport, gradient),
                    test_path(1, bounds(179.5, 132.25, 38.25, 29.5), viewport, gradient),
                ],
            ),
            ("full window", vec![test_path(1, viewport, viewport, gradient)]),
            (
                "off window",
                vec![test_path(1, bounds(-50., -50., 10., 10.), viewport, gradient)],
            ),
            (
                "empty mask",
                vec![test_path(
                    1,
                    bounds(40., 40., 30., 30.),
                    bounds(40., 40., 0., 0.),
                    gradient,
                )],
            ),
        ];
        for (case, paths) in cases {
            let scene = test_scene(paths, viewport);
            for appearance in [
                WindowBackgroundAppearance::Opaque,
                WindowBackgroundAppearance::Transparent,
            ] {
                region.release_path_scratch();
                let expected = full_window_pixels(&mut full, &scene, appearance)?;
                region.draw_scene(&scene, appearance)?;
                assert_same_pixels(&pixels(&region)?, &expected, width as u32, case);
                assert_eq!(region.resources.as_ref().unwrap().viewport.TopLeftX, 0.);
                assert_eq!(region.resources.as_ref().unwrap().viewport.TopLeftY, 0.);
                if matches!(case, "off window" | "empty mask") {
                    assert!(region.resources.as_ref().unwrap().path_scratch.is_none());
                }
            }
        }
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn warp_path_capacity_grows_and_decays_by_frame() -> Result<()> {
    let (mut region, _region_window) = warp_renderer()?;
    let (mut full, _full_window) = full_window_renderer()?;
    let width = 513;
    let height = 385;
    region.resize(size(DevicePixels(width), DevicePixels(height)))?;
    full.resize(size(DevicePixels(width), DevicePixels(height)))?;
    let viewport = bounds(0., 0., width as f32, height as f32);
    let color = hsla(0.6, 0.8, 0.5, 0.6).into();
    let small_path = test_path(1, bounds(113.25, 91.5, 29.5, 23.25), viewport, color);
    let large_path = test_path(3, bounds(21.25, 31.5, 282.5, 208.25), viewport, color);
    let small = test_scene(vec![small_path.clone()], viewport);
    let large = test_scene(vec![large_path.clone()], viewport);
    let tall = test_scene(
        vec![test_path(1, bounds(40.25, 30.5, 20.5, 280.25), viewport, color)],
        viewport,
    );
    for (scene, capacity) in [
        (&small, (64, 64)),
        (&large, (320, 256)),
        (&tall, (320, 320)),
    ] {
        let expected = full_window_pixels(&mut full, scene, WindowBackgroundAppearance::Transparent)?;
        region.draw_scene(scene, WindowBackgroundAppearance::Transparent)?;
        assert_capacity(&region, capacity);
        assert_same_pixels(&pixels(&region)?, &expected, width as u32, "capacity growth");
    }
    let texture = scratch(&region).texture.clone();
    let mut mixed = test_scene(vec![small_path, large_path], viewport);
    mixed
        .quads
        .push(quad(2, bounds(12., 12., 2., 2.), hsla(0., 0., 1., 0.5)));
    mixed.finish();
    assert_eq!(
        mixed
            .batches()
            .filter(|batch| matches!(batch, PrimitiveBatch::Paths(_)))
            .count(),
        2,
    );
    let mixed_expected = full_window_pixels(&mut full, &mixed, WindowBackgroundAppearance::Transparent)?;
    for _ in 0..SCRATCH_RELEASE_AFTER_FRAMES + 1 {
        region.draw_scene(&mixed, WindowBackgroundAppearance::Transparent)?;
        assert_eq!(scratch(&region).texture, texture);
        assert_eq!(scratch(&region).low_frames, 0);
    }
    assert_same_pixels(&pixels(&region)?, &mixed_expected, width as u32, "whole frame maximum");
    let expected = full_window_pixels(&mut full, &small, WindowBackgroundAppearance::Transparent)?;
    // A large frame interrupts the first run of 29 small frames.
    for _ in 0..SCRATCH_RELEASE_AFTER_FRAMES - 1 {
        region.draw_scene(&small, WindowBackgroundAppearance::Transparent)?;
        assert_eq!(scratch(&region).texture, texture);
    }
    region.draw_scene(&mixed, WindowBackgroundAppearance::Transparent)?;
    assert_eq!(scratch(&region).texture, texture);
    assert_eq!(scratch(&region).low_frames, 0);
    for frame in 1..SCRATCH_RELEASE_AFTER_FRAMES {
        region.draw_scene(&small, WindowBackgroundAppearance::Transparent)?;
        assert_eq!(scratch(&region).texture, texture);
        assert_eq!(scratch(&region).low_frames, frame);
    }
    drop(texture);
    region.draw_scene(&small, WindowBackgroundAppearance::Transparent)?;
    assert_capacity(&region, (64, 64));
    assert_same_pixels(&pixels(&region)?, &expected, width as u32, "capacity shrink");
    let texture = scratch(&region).texture.clone();
    for frame in 0..64 {
        let scene = test_scene(
            vec![test_path(
                1,
                bounds(43.25 + frame as f32 * 1.25, 54.5, 29.5, 23.25),
                viewport,
                color,
            )],
            viewport,
        );
        region.draw_scene(&scene, WindowBackgroundAppearance::Transparent)?;
        let expected = full_window_pixels(&mut full, &scene, WindowBackgroundAppearance::Transparent)?;
        assert_same_pixels(&pixels(&region)?, &expected, width as u32, "moving origin");
        assert_eq!(scratch(&region).texture, texture);
    }
    drop(texture);
    Ok(())
}

#[::core::prelude::v1::test]
fn warp_path_capacity_survives_resize_and_clamps_to_window() -> Result<()> {
    let (mut region, _region_window) = warp_renderer()?;
    let (mut full, _full_window) = full_window_renderer()?;
    let viewport = bounds(0., 0., 400., 300.);
    let scene = test_scene(
        vec![test_path(
            1,
            bounds(21.25, 31.5, 20.5, 23.25),
            viewport,
            hsla(0.3, 0.7, 0.4, 0.5).into(),
        )],
        viewport,
    );
    region.resize(size(DevicePixels(400), DevicePixels(300)))?;
    region.draw_scene(&scene, WindowBackgroundAppearance::Transparent)?;
    assert_capacity(&region, (64, 64));
    let texture = scratch(&region).texture.clone();
    region.resize(size(DevicePixels(500), DevicePixels(350)))?;
    assert_eq!(scratch(&region).texture, texture);
    region.resize(size(DevicePixels(100), DevicePixels(80)))?;
    assert_eq!(scratch(&region).texture, texture);
    full.resize(size(DevicePixels(100), DevicePixels(80)))?;
    let expected = full_window_pixels(&mut full, &scene, WindowBackgroundAppearance::Transparent)?;
    region.draw_scene(&scene, WindowBackgroundAppearance::Transparent)?;
    assert_same_pixels(&pixels(&region)?, &expected, 100, "reuse after resize");
    drop(texture);
    region.resize(size(DevicePixels(47), DevicePixels(39)))?;
    assert!(region.resources.as_ref().unwrap().path_scratch.is_none());
    let viewport = bounds(0., 0., 47., 39.);
    let scene = test_scene(
        vec![test_path(1, viewport, viewport, hsla(0.1, 0.9, 0.6, 0.5).into())],
        viewport,
    );
    region.draw_scene(&scene, WindowBackgroundAppearance::Transparent)?;
    assert_capacity(&region, (47, 39));
    full.resize(size(DevicePixels(47), DevicePixels(39)))?;
    let expected = full_window_pixels(&mut full, &scene, WindowBackgroundAppearance::Transparent)?;
    assert_same_pixels(&pixels(&region)?, &expected, 47, "window clamping");
    Ok(())
}
