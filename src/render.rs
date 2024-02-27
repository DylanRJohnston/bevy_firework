use crate::plugin::PARTICLE_SHADER_HANDLE;

use super::core::{ParticleData, ParticleSpawnerData, ParticleSpawnerSettings};
use bevy::{
    core_pipeline::{
        core_3d::{Transparent3d, CORE_3D_DEPTH_FORMAT},
        prepass::{
            DeferredPrepass, DepthPrepass, MotionVectorPrepass, NormalPrepass, ViewPrepassTextures,
        },
    },
    ecs::{
        query::QueryItem,
        system::{lifetimeless::*, SystemParamItem},
    },
    pbr::{MeshPipeline, MeshPipelineKey, RenderMeshInstances, SetMeshViewBindGroup},
    prelude::*,
    render::{
        extract_component::{
            ComponentUniforms, DynamicUniformIndex, ExtractComponent, ExtractComponentPlugin,
            UniformComponentPlugin,
        },
        primitives::Aabb,
        render_asset::RenderAssets,
        render_phase::{
            AddRenderCommand, DrawFunctions, PhaseItem, RenderCommand, RenderCommandResult,
            RenderPhase, SetItemPipeline, TrackedRenderPass,
        },
        render_resource::*,
        renderer::RenderDevice,
        texture::BevyDefault,
        view::{ExtractedView, ViewTarget},
        Render, RenderApp, RenderSet,
    },
};
use bytemuck::{Pod, Zeroable};

#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct ParticleInstance {
    position: Vec3,
    scale: f32,
    color: [f32; 4],
}

impl From<&ParticleData> for ParticleInstance {
    fn from(value: &ParticleData) -> Self {
        Self {
            position: value.position,
            scale: value.scale,
            color: value.color.into(),
        }
    }
}

#[derive(Component, Deref)]
pub struct ParticleMaterialData {
    #[deref]
    particles: Vec<ParticleInstance>,
    alpha_mode: AlphaMode,
}

impl ExtractComponent for ParticleSpawnerData {
    type Query = (
        &'static ParticleSpawnerData,
        &'static ParticleSpawnerSettings,
    );
    type Filter = ();
    type Out = (ParticleMaterialData, FireworkUniform);

    fn extract_component(item: QueryItem<'_, Self::Query>) -> Option<Self::Out> {
        let (data, settings) = item;
        Some((
            ParticleMaterialData {
                particles: data.particles.iter().map(|p| p.into()).collect(),
                alpha_mode: settings.blend_mode.into(),
            },
            FireworkUniform {
                alpha_mode: settings.blend_mode.into(),
                pbr: settings.pbr.into(),
                fade_edge: settings.fade_edge,
                fade_scene: settings.fade_scene,
            },
        ))
    }
}

#[derive(Resource)]
pub struct DummyDepthTexture(pub TextureView);

pub struct CustomMaterialPlugin;

impl Plugin for CustomMaterialPlugin {
    fn build(&self, app: &mut App) {
        app //
            .add_plugins(ExtractComponentPlugin::<ParticleSpawnerData>::default())
            .add_plugins(UniformComponentPlugin::<FireworkUniform>::default())
            .add_systems(Last, update_aabbs);
        app.sub_app_mut(RenderApp)
            .add_render_command::<Transparent3d, DrawCustom>()
            .init_resource::<SpecializedRenderPipelines<FireworkPipeline>>()
            .add_systems(
                Render,
                (
                    queue_custom.in_set(RenderSet::QueueMeshes),
                    prepare_instance_buffers.in_set(RenderSet::PrepareResources),
                    prepare_firework_bindgroup.in_set(RenderSet::PrepareBindGroups),
                ),
            );
    }

    fn finish(&self, app: &mut App) {
        let msaa_samples = app
            .world
            .get_resource::<Msaa>()
            .map(|msaa| msaa.samples())
            .unwrap_or(1);

        let render_app = app.sub_app_mut(RenderApp);
        let firework_uniform_layout =
            FireworkUniformBindgroupLayout::create(render_app.world.resource::<RenderDevice>());
        let dummy_texture = render_app
            .world
            .resource::<RenderDevice>()
            .create_texture(&TextureDescriptor {
                label: Some("Dummy Depth Texture"),
                size: Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: msaa_samples,
                dimension: TextureDimension::D2,
                format: TextureFormat::Depth32Float,
                usage: TextureUsages::TEXTURE_BINDING
                    | TextureUsages::RENDER_ATTACHMENT
                    | TextureUsages::COPY_SRC,
                view_formats: &[],
            })
            .create_view(&TextureViewDescriptor {
                label: None,
                format: None,
                dimension: None,
                aspect: TextureAspect::DepthOnly,
                base_mip_level: 0,
                mip_level_count: Some(1),
                base_array_layer: 0,
                array_layer_count: Some(1),
            });
        render_app.insert_resource(DummyDepthTexture(dummy_texture));
        render_app.insert_resource(firework_uniform_layout);
        render_app.init_resource::<FireworkPipeline>();
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn queue_custom(
    transparent_3d_draw_functions: Res<DrawFunctions<Transparent3d>>,
    custom_pipeline: Res<FireworkPipeline>,
    msaa: Res<Msaa>,
    mut pipelines: ResMut<SpecializedRenderPipelines<FireworkPipeline>>,
    pipeline_cache: Res<PipelineCache>,
    meshes: Res<RenderAssets<Mesh>>,
    render_mesh_instances: Res<RenderMeshInstances>,
    material_meshes: Query<(Entity, &ParticleMaterialData)>,
    mut views: Query<(
        &ExtractedView,
        &mut RenderPhase<Transparent3d>,
        (
            Has<NormalPrepass>,
            Has<DepthPrepass>,
            Has<MotionVectorPrepass>,
            Has<DeferredPrepass>,
        ),
    )>,
) {
    let draw_custom = transparent_3d_draw_functions.read().id::<DrawCustom>();

    let msaa_key = MeshPipelineKey::from_msaa_samples(msaa.samples());

    for (
        view,
        mut transparent_phase,
        (normal_prepass, depth_prepass, motion_vector_prepass, deferred_prepass),
    ) in &mut views
    {
        let view_key = msaa_key | MeshPipelineKey::from_hdr(view.hdr);
        let rangefinder = view.rangefinder3d();
        for (entity, particle_material_data) in &material_meshes {
            let Some(mesh_instance) = render_mesh_instances.get(&entity) else {
                continue;
            };
            let Some(mesh) = meshes.get(mesh_instance.mesh_asset_id) else {
                continue;
            };
            let mut key =
                view_key | MeshPipelineKey::from_primitive_topology(mesh.primitive_topology);
            match particle_material_data.alpha_mode {
                AlphaMode::Blend => {
                    key |= MeshPipelineKey::BLEND_ALPHA;
                }
                AlphaMode::Premultiplied | AlphaMode::Add => {
                    // Premultiplied and Add share the same pipeline key
                    // They're made distinct in the PBR shader, via `premultiply_alpha()`
                    key |= MeshPipelineKey::BLEND_PREMULTIPLIED_ALPHA;
                }
                AlphaMode::Multiply => {
                    key |= MeshPipelineKey::BLEND_MULTIPLY;
                }
                AlphaMode::Mask(_) => {
                    key |= MeshPipelineKey::MAY_DISCARD;
                }
                _ => (),
            };

            if normal_prepass {
                key |= MeshPipelineKey::NORMAL_PREPASS;
            }

            if depth_prepass {
                key |= MeshPipelineKey::DEPTH_PREPASS;
            }

            if motion_vector_prepass {
                key |= MeshPipelineKey::MOTION_VECTOR_PREPASS;
            }

            if deferred_prepass {
                key |= MeshPipelineKey::DEFERRED_PREPASS;
            }

            let pipeline = pipelines.specialize(&pipeline_cache, &custom_pipeline, key);
            transparent_phase.add(Transparent3d {
                entity,
                pipeline,
                draw_function: draw_custom,
                distance: rangefinder
                    .distance_translation(&mesh_instance.transforms.transform.translation),
                batch_range: 0..1,
                dynamic_offset: None,
            });
        }
    }
}

#[derive(Component)]
pub struct InstanceBuffer {
    buffer: Buffer,
    length: usize,
}

fn prepare_instance_buffers(
    mut commands: Commands,
    query: Query<(Entity, &ParticleMaterialData)>,
    render_device: Res<RenderDevice>,
) {
    for (entity, instance_data) in &query {
        let buffer = render_device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("instance data buffer"),
            contents: bytemuck::cast_slice(instance_data.particles.as_slice()),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        });
        commands.entity(entity).insert(InstanceBuffer {
            buffer,
            length: instance_data.len(),
        });
    }
}

#[derive(Resource)]
pub struct FireworkUniformBindgroupLayout {
    pub layout: BindGroupLayout,
}

impl FireworkUniformBindgroupLayout {
    pub fn create(render_device: &RenderDevice) -> Self {
        let layout = render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("Firework Uniform Layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX_FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: Some(FireworkUniform::min_size()),
                    },
                    count: None,
                },
                // The depth texture
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Depth,
                        view_dimension: TextureViewDimension::D2,
                        multisampled: true,
                    },
                    count: None,
                },
            ],
        });

        Self { layout }
    }
}

#[derive(Component)]
pub struct FireworkUniformBindgroup {
    bindgroup: BindGroup,
}

#[derive(Component, ShaderType, Clone, Debug)]
pub struct FireworkUniform {
    alpha_mode: u32,
    pbr: u32,
    fade_edge: f32,
    fade_scene: f32,
}

pub fn prepare_firework_bindgroup(
    mut commands: Commands,
    firework_uniform_bindgroup_layout: Res<FireworkUniformBindgroupLayout>,
    render_device: Res<RenderDevice>,
    firework_uniforms: Res<ComponentUniforms<FireworkUniform>>,
    dummy_depth_texture: Res<DummyDepthTexture>,
    view_query: Query<(Entity, Option<&ViewPrepassTextures>), With<ViewTarget>>,
) {
    if let Some(binding) = firework_uniforms.uniforms().binding() {
        for (entity, view_prepass_textures_opt) in &view_query {
            let mut entries = vec![BindGroupEntry {
                binding: 0,
                resource: binding.clone(),
            }];
            if let Some(depth) = view_prepass_textures_opt.and_then(|vpt| vpt.depth.as_ref()) {
                entries.push(BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::TextureView(&depth.default_view),
                });
            } else {
                // Push a dummy depth texture view
                entries.push(BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::TextureView(&dummy_depth_texture.0),
                });
            }
            commands.entity(entity).insert(FireworkUniformBindgroup {
                bindgroup: render_device.create_bind_group(
                    "Firework Uniform Bindgroup",
                    &firework_uniform_bindgroup_layout.layout,
                    &entries,
                ),
            });
        }
    }
}

fn update_aabbs(mut query: Query<(&mut Aabb, &GlobalTransform, &ParticleSpawnerData)>) {
    for (mut aabb, global_transform, spawner_data) in &mut query {
        if spawner_data.particles.is_empty() {
            continue;
        }
        let min = spawner_data
            .particles
            .iter()
            .map(|p| p.position - Vec3::splat(p.scale))
            .fold(Vec3::MAX, |acc, v| acc.min(v));
        let max = spawner_data
            .particles
            .iter()
            .map(|p| p.position + Vec3::splat(p.scale))
            .fold(Vec3::MIN, |acc, v| acc.max(v));
        let center = (min + max) / 2.;
        let half_extents = (max - min) / 2.;
        aabb.center = global_transform
            .compute_matrix()
            .inverse()
            .transform_point3(center)
            .into();
        aabb.half_extents = half_extents.into();
    }
}

#[derive(Resource)]
pub struct FireworkPipeline {
    vertex_shader: Handle<Shader>,
    mesh_pipeline: MeshPipeline,
    uniform_layout: BindGroupLayout,
}

impl FromWorld for FireworkPipeline {
    fn from_world(world: &mut World) -> Self {
        let vertex_shader = PARTICLE_SHADER_HANDLE;
        let mesh_pipeline = world.resource::<MeshPipeline>();

        FireworkPipeline {
            vertex_shader,
            mesh_pipeline: mesh_pipeline.clone(),
            uniform_layout: world
                .resource::<FireworkUniformBindgroupLayout>()
                .layout
                .clone(),
        }
    }
}

impl SpecializedRenderPipeline for FireworkPipeline {
    type Key = MeshPipelineKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        let view_layout = self.mesh_pipeline.get_view_layout(key.into()).clone();
        let layout = vec![view_layout, self.uniform_layout.clone()];
        let format = if key.contains(MeshPipelineKey::HDR) {
            ViewTarget::TEXTURE_FORMAT_HDR
        } else {
            TextureFormat::bevy_default()
        };

        let mut shader_defs = vec!["MESH_BINDGROUP_1".into(), "VERTEX_UVS".into()];

        if key.msaa_samples() > 1 {
            shader_defs.push("MULTISAMPLED".into());
        }
        if key.contains(MeshPipelineKey::DEPTH_PREPASS) {
            shader_defs.push("DEPTH_PREPASS".into());
        }

        // Ok(descriptor)
        RenderPipelineDescriptor {
            label: Some("Firework Pipeline".into()),
            layout,
            push_constant_ranges: vec![],
            vertex: VertexState {
                shader: self.vertex_shader.clone(),
                // meshes typically live in bind group 2. because we are using bindgroup 1
                // we need to add MESH_BINDGROUP_1 shader def so that the bindings are correctly
                // linked in the shader
                shader_defs: shader_defs.clone(),
                entry_point: "vertex".into(),
                buffers: vec![VertexBufferLayout {
                    array_stride: std::mem::size_of::<ParticleInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: vec![
                        VertexAttribute {
                            format: VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 3, // shader locations 0-2 are taken up by Position, Normal and UV attributes
                        },
                        VertexAttribute {
                            format: VertexFormat::Float32x4,
                            offset: VertexFormat::Float32x4.size(),
                            shader_location: 4,
                        },
                        VertexAttribute {
                            format: VertexFormat::Uint32,
                            offset: VertexFormat::Float32x4.size() * 2,
                            shader_location: 5,
                        },
                    ],
                }],
            },
            fragment: Some(FragmentState {
                shader: self.vertex_shader.clone(),
                // meshes typically live in bind group 2. because we are using bindgroup 1
                // we need to add MESH_BINDGROUP_1 shader def so that the bindings are correctly
                // linked in the shader
                shader_defs,
                entry_point: "fragment".into(),
                targets: vec![Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: Some(DepthStencilState {
                format: CORE_3D_DEPTH_FORMAT,
                depth_write_enabled: false,
                // Bevy uses reverse-Z, so Greater really means closer
                depth_compare: CompareFunction::Greater,
                stencil: StencilState::default(),
                bias: DepthBiasState::default(),
            }),
            multisample: MultisampleState {
                count: key.msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
        }
    }
}

type DrawCustom = (
    SetItemPipeline,
    SetMeshViewBindGroup<0>,
    SetFireworkBindGroup<1>,
    DrawFirework,
);

pub struct SetFireworkBindGroup<const I: usize>;
impl<const I: usize, P: PhaseItem> RenderCommand<P> for SetFireworkBindGroup<I> {
    type Param = ();
    type ViewWorldQuery = &'static FireworkUniformBindgroup;
    type ItemWorldQuery = Read<DynamicUniformIndex<FireworkUniform>>;

    fn render<'w>(
        _item: &P,
        firework_bindgroup: bevy::ecs::query::ROQueryItem<'w, Self::ViewWorldQuery>,
        uniform_index: bevy::ecs::query::ROQueryItem<'w, Self::ItemWorldQuery>,
        _param: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(I, &firework_bindgroup.bindgroup, &[uniform_index.index()]);
        RenderCommandResult::Success
    }
}

pub struct DrawFirework;

impl<P: PhaseItem> RenderCommand<P> for DrawFirework {
    type Param = ();
    type ViewWorldQuery = ();
    type ItemWorldQuery = Read<InstanceBuffer>;

    #[inline]
    fn render<'w>(
        _item: &P,
        _view: (),
        instance_buffer: &'w InstanceBuffer,
        _: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_vertex_buffer(0, instance_buffer.buffer.slice(..));

        pass.draw(0..6, 0..instance_buffer.length as u32);
        RenderCommandResult::Success
    }
}
