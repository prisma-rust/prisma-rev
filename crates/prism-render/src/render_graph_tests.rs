use super::*;

#[test]
fn resource_handle_invalid() {
    assert_eq!(ResourceHandle::INVALID.0, u32::MAX);
}

#[test]
fn builder_creates_resources() {
    let mut builder = RenderGraphBuilder::new();
    let h = builder.create_resource(ResourceType::ColorAttachment {
        format: vk::Format::A2B10G10R10_UNORM_PACK32,
        extent: vk::Extent2D {
            width: 1920,
            height: 1080,
        },
        sample_count: vk::SampleCountFlags::TYPE_1,
    });
    assert_eq!(h.0, 0);
    assert!(builder.resources.contains_key(&h));
}

#[test]
fn settings_default_is_high_precision_gbuffer() {
    // P0 默认 flipped to `true`: world-space normals from 法线
    // maps need Rgba16F precision in GBuffer A. See Plan §4.3.
    let s = RenderSettings::default();
    assert!(s.gbuffer_high_precision);
    assert!(!s.ray_tracing_enabled);
    assert_eq!(s.ray_query_resolution_scale, 0.5);
    assert_eq!(s.sharc_capacity, 1 << 20);
}

fn draw_item(queue: i32, eye_distance_x: f32) -> DrawItem {
    // 用一个只有平移的模型矩阵，`world_position()` 读到的是 x 轴位置。
    let mut model = [[0.0f32; 4]; 4];
    for (i, column) in model.iter_mut().enumerate() {
        column[i] = 1.0;
    }
    model[3][0] = eye_distance_x;
    DrawItem {
        mesh: crate::managers::MeshHandle::default(),
        model,
        material: None,
        cast_shadow: true,
        visible_in_main_pass: true,
        cast_shadow_two_sided: false,
        state: DrawState {
            render_queue: queue,
            ..DrawState::default()
        },
    }
}

#[test]
fn draw_state_defaults_to_opaque() {
    let state = DrawState::default();
    assert_eq!(state.cull_mode, prism_gfx::CullMode::Back);
    assert_eq!(state.render_queue, prism_gfx::RenderQueue::GEOMETRY);
    assert!(state.z_write);
    assert!(!state.blends(), "One/Zero/Add 是不透明，必须关掉混合");

    let transparent = DrawState {
        src_blend: prism_gfx::BlendMode::SrcAlpha,
        dst_blend: prism_gfx::BlendMode::OneMinusSrcAlpha,
        ..DrawState::default()
    };
    assert!(transparent.blends());
}

#[test]
fn sort_draw_items_orders_opaque_first_then_transparent_back_to_front() {
    let eye = [0.0, 0.0, 0.0];
    let mut items = vec![
        // 透明段：近的放前面，排序后必须变成远 -> 近。
        draw_item(prism_gfx::RenderQueue::TRANSPARENT, 1.0),
        // 不透明但队列更大的要排在不透明段后面。
        draw_item(prism_gfx::RenderQueue::ALPHA_TEST, 0.0),
        draw_item(prism_gfx::RenderQueue::GEOMETRY, 0.0),
        draw_item(prism_gfx::RenderQueue::TRANSPARENT, 9.0),
        draw_item(prism_gfx::RenderQueue::BACKGROUND, 0.0),
    ];
    sort_draw_items(&mut items, eye);

    let queues: Vec<i32> = items.iter().map(|i| i.state.render_queue).collect();
    assert_eq!(
        queues,
        vec![
            prism_gfx::RenderQueue::BACKGROUND,
            prism_gfx::RenderQueue::GEOMETRY,
            prism_gfx::RenderQueue::ALPHA_TEST,
            prism_gfx::RenderQueue::TRANSPARENT,
            prism_gfx::RenderQueue::TRANSPARENT,
        ]
    );
    // 透明段内部按距离从远到近（x = 9 先于 x = 1）。
    assert_eq!(items[3].world_position()[0], 9.0);
    assert_eq!(items[4].world_position()[0], 1.0);
}

#[test]
fn sort_draw_items_is_stable_for_equal_queues() {
    let eye = [0.0, 0.0, 0.0];
    let mut items = vec![
        draw_item(prism_gfx::RenderQueue::GEOMETRY, 5.0),
        draw_item(prism_gfx::RenderQueue::GEOMETRY, 1.0),
    ];
    sort_draw_items(&mut items, eye);
    // 不透明段不按距离重排，保持提交顺序。
    assert_eq!(items[0].world_position()[0], 5.0);
    assert_eq!(items[1].world_position()[0], 1.0);
}

#[test]
fn lazy_allocation_requires_a_pure_transient_attachment() {
    // ForwardPass color: also STORAGE, so it can never be lazy.
    assert!(!lazy_allocation_allowed(
        vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::INPUT_ATTACHMENT
            | vk::ImageUsageFlags::STORAGE
    ));
    // Forward depth + shadow map: SAMPLED by later passes, so never lazy.
    assert!(!lazy_allocation_allowed(
        vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
            | vk::ImageUsageFlags::INPUT_ATTACHMENT
            | vk::ImageUsageFlags::SAMPLED
    ));
    // Storage images can not be lazy either.
    assert!(!lazy_allocation_allowed(
        vk::ImageUsageFlags::STORAGE
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED
    ));
    // A pure transient attachment (write-only, discarded) may be lazy.
    assert!(lazy_allocation_allowed(
        vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::TRANSIENT_ATTACHMENT
    ));
}

struct TestPass {
    name: &'static str,
    reads: Vec<ResourceUsage>,
    writes: Vec<ResourceUsage>,
}

impl RenderPassNode for TestPass {
    fn name(&self) -> &str {
        self.name
    }

    fn setup(&mut self, graph: &mut RenderGraphBuilder, _settings: &RenderSettings) {
        for r in &self.reads {
            graph.read_usage(r.clone());
        }
        for w in &self.writes {
            graph.write_usage(w.clone());
        }
    }

    fn execute(&mut self, _ctx: &RenderContext, _resources: &mut GraphResources) -> Result<()> {
        Ok(())
    }
}

#[test]
fn graph_precomputes_and_caches_pass_edges_and_last_writers() {
    let mut builder = RenderGraphBuilder::new();
    let res_a = ResourceHandle(0);
    let res_b = ResourceHandle(1);

    let mut pass0 = TestPass {
        name: "Pass0",
        reads: vec![],
        writes: vec![ResourceUsage {
            handle: res_a,
            access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }],
    };
    pass0.setup(&mut builder, &RenderSettings::default());
    builder.add_pass(Box::new(pass0));

    let mut pass1 = TestPass {
        name: "Pass1",
        reads: vec![ResourceUsage {
            handle: res_a,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }],
        writes: vec![ResourceUsage {
            handle: res_b,
            access: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            stage: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }],
    };
    pass1.setup(&mut builder, &RenderSettings::default());
    builder.add_pass(Box::new(pass1));
    let mut graph = builder.build();

    // Verify pass_edges has precomputed buckets
    assert_eq!(graph.pass_edges.len(), 2);
    assert_eq!(graph.pass_edges[0].len(), 1);
    assert_eq!(graph.pass_edges[0][0].pass_idx, 0);
    assert_eq!(graph.pass_edges[0][0].kind, EdgeKind::Write);
    assert_eq!(graph.pass_edges[0][0].usage.handle, res_a);

    assert_eq!(graph.pass_edges[1].len(), 2);
    assert_eq!(graph.pass_edges[1][0].pass_idx, 1);
    assert_eq!(graph.pass_edges[1][0].kind, EdgeKind::Read);
    assert_eq!(graph.pass_edges[1][0].usage.handle, res_a);
    assert_eq!(graph.pass_edges[1][1].pass_idx, 1);
    assert_eq!(graph.pass_edges[1][1].kind, EdgeKind::Write);
    assert_eq!(graph.pass_edges[1][1].usage.handle, res_b);

    // Verify last_writers
    assert_eq!(graph.last_writers.len(), 2);
    assert_eq!(
        graph.last_writers.get(&res_a).unwrap().access,
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE
    );
    assert_eq!(
        graph.last_writers.get(&res_b).unwrap().access,
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE
    );

    // Now test add_pass updates precomputed cache
    let pass2 = Box::new(TestPass {
        name: "Pass2",
        reads: vec![ResourceUsage {
            handle: res_b,
            access: vk::AccessFlags::SHADER_READ,
            stage: vk::PipelineStageFlags::FRAGMENT_SHADER,
            layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }],
        writes: vec![ResourceUsage {
            handle: res_a, // writes res_a again, updating last_writers
            access: vk::AccessFlags::TRANSFER_WRITE,
            stage: vk::PipelineStageFlags::TRANSFER,
            layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        }],
    });

    graph.add_pass(pass2);
    assert_eq!(graph.pass_edges.len(), 3);
    assert_eq!(graph.pass_edges[2].len(), 2);
    assert_eq!(graph.pass_edges[2][0].pass_idx, 2);
    assert_eq!(graph.pass_edges[2][1].pass_idx, 2);

    // last_writers for res_a should now reflect Pass2
    assert_eq!(
        graph.last_writers.get(&res_a).unwrap().access,
        vk::AccessFlags::TRANSFER_WRITE
    );
    assert_eq!(
        graph.last_writers.get(&res_a).unwrap().stage,
        vk::PipelineStageFlags::TRANSFER
    );
}

#[test]
fn graph_resources_clear_frame_preserves_persistent_resources() {
    let mut resources = GraphResources::new();
    let handle = ResourceHandle(42);
    resources.resources.insert(
        handle,
        GraphResource {
            handle,
            res_type: ResourceType::StorageBuffer { size: 1024 },
            image: None,
            image_view: None,
            memory: None,
        },
    );

    // Simulate per-frame published data
    resources.set_image_view(handle, vk::ImageView::null());
    resources.set_image(handle, vk::Image::null());
    resources.set_param(handle, 123);

    assert!(resources.published_view(handle).is_some());
    assert!(resources.published_image(handle).is_some());
    assert_eq!(resources.param(handle), Some(123));

    resources.clear_frame();

    // Persistent resources table must be preserved
    assert!(resources.resources.contains_key(&handle));
    // Per-frame items must be cleared
    assert!(resources.published_view(handle).is_none());
    assert!(resources.published_image(handle).is_none());
    assert_eq!(resources.param(handle), None);
}

#[test]
fn clip_bounds_normalises_and_disables_degenerate_rects() {
    // 作者可以任意顺序写角点。
    assert_eq!(
        clip_bounds(true, [10.0, 20.0, -5.0, 4.0]),
        [-5.0, 4.0, 10.0, 20.0]
    );
    // 关闭 / 非有限 / 零面积 → 不裁剪（刚勾上开关还没填矩形时不会隐藏精灵）。
    assert_eq!(clip_bounds(false, [0.0, 0.0, 8.0, 8.0]), NO_CLIP);
    assert_eq!(clip_bounds(true, [0.0, 0.0, f32::NAN, 8.0]), NO_CLIP);
    assert_eq!(clip_bounds(true, [4.0, 0.0, 4.0, 8.0]), NO_CLIP);
    assert_eq!(clip_bounds(true, [0.0, 0.0, 0.0, 0.0]), NO_CLIP);
    // 半平面式裁剪（水面以下的倒影）保持有效：`max < min` 才是关。
    let below = clip_bounds(true, [-1.0e4, -1.0e4, 1.0e4, 12.0]);
    assert_eq!(below, [-1.0e4, -1.0e4, 1.0e4, 12.0]);
    assert!(below[2] >= below[0] && below[3] >= below[1]);
}

/// #613：内部分辨率只在两个轴都有效时生效，单轴时按窗口宽高比推导。
#[test]
fn internal_extent_derives_and_clamps() {
    let swapchain = vk::Extent2D {
        width: 2000,
        height: 1125,
    };
    assert_eq!(internal_extent(swapchain, [0, 0]), None);
    assert_eq!(
        internal_extent(swapchain, [640, 360]),
        Some(vk::Extent2D {
            width: 640,
            height: 360
        })
    );
    // 只给宽：高度按窗口比例推出来（640 / (2000/1125) = 360）。
    assert_eq!(
        internal_extent(swapchain, [640, 0]),
        Some(vk::Extent2D {
            width: 640,
            height: 360
        })
    );
    // 空窗口不能算出 0 尺寸的目标。
    let tiny = vk::Extent2D {
        width: 0,
        height: 0,
    };
    let derived = internal_extent(tiny, [640, 0]).expect("width only");
    assert!(derived.width >= 1 && derived.height >= 1, "{derived:?}");
}

/// #613：整数缩放取整到最大整数倍并居中，stretch 铺满窗口。
#[test]
fn post_destination_centres_an_integer_upscale() {
    let swapchain = vk::Extent2D {
        width: 2000,
        height: 1125,
    };
    let internal = vk::Extent2D {
        width: 640,
        height: 360,
    };

    let stretched = post_destination(swapchain, internal, InternalScaleMode::Stretch);
    assert_eq!(stretched.offset.x, 0);
    assert_eq!(stretched.offset.y, 0);
    assert_eq!(stretched.extent.width, 2000);
    assert_eq!(stretched.extent.height, 1125);

    // min(2000/640, 1125/360) = min(3.125, 3.125) → 3 倍，1920x1080，居中。
    let integer = post_destination(swapchain, internal, InternalScaleMode::Integer);
    assert_eq!(integer.extent.width, 1920);
    assert_eq!(integer.extent.height, 1080);
    assert_eq!(integer.offset.x, 40);
    assert_eq!(integer.offset.y, 22);

    // 内部分辨率比窗口还大时不会有 0 倍：退化成 1 倍并居中裁切。
    let oversized = post_destination(
        swapchain,
        vk::Extent2D {
            width: 4000,
            height: 2250,
        },
        InternalScaleMode::Integer,
    );
    assert!(oversized.extent.width >= 1 && oversized.extent.height >= 1);
    assert_eq!(oversized.offset.x, 0);
}

#[test]
fn internal_scale_mode_parses_authoring_values() {
    assert_eq!(
        InternalScaleMode::parse("stretch"),
        Some(InternalScaleMode::Stretch)
    );
    assert_eq!(
        InternalScaleMode::parse(" Fill "),
        Some(InternalScaleMode::Stretch)
    );
    assert_eq!(
        InternalScaleMode::parse("integer"),
        Some(InternalScaleMode::Integer)
    );
    assert_eq!(
        InternalScaleMode::parse("Nearest"),
        Some(InternalScaleMode::Integer)
    );
    assert_eq!(
        InternalScaleMode::parse("pixel"),
        Some(InternalScaleMode::Integer)
    );
    assert_eq!(InternalScaleMode::parse("bogus"), None);
    assert_eq!(InternalScaleMode::default().as_str(), "stretch");
    assert_eq!(InternalScaleMode::Integer.as_str(), "integer");
}
