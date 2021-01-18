use crate::{
    asset::{
        image::RawRgba8Image,
        mesh::{PackedTriangleMesh, PackedVertex, TexParams},
    },
    backend::{self, image::*, shader::*, RenderBackend},
    dynamic_constants::DynamicConstants,
    image_lut::{ComputeImageLut, ImageLut},
    render_passes::{RasterMeshesData, UploadedTriMesh},
    renderer::*,
    rg,
    rg::RetiredRenderGraph,
    viewport::ViewConstants,
    FrameState,
};
use backend::buffer::{Buffer, BufferDesc};
use glam::Vec2;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use slingshot::{
    ash::{
        version::DeviceV1_0,
        vk::{self, ImageView},
    },
    backend::{
        device,
        ray_tracing::{
            RayTracingAcceleration, RayTracingBottomAccelerationDesc, RayTracingGeometryDesc,
            RayTracingGeometryPart, RayTracingGeometryType, RayTracingTopAccelerationDesc,
        },
    },
    rspirv_reflect, vk_sync,
};
use std::{collections::HashMap, mem::size_of, sync::Arc};
use winit::VirtualKeyCode;

#[repr(C)]
#[derive(Copy, Clone)]
struct FrameConstants {
    view_constants: ViewConstants,
    mouse: [f32; 4],
    frame_idx: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct GpuMesh {
    vertex_core_offset: u32,
    vertex_uv_offset: u32,
    vertex_mat_offset: u32,
    vertex_aux_offset: u32,
    mat_data_offset: u32,
    index_offset: u32,
}

const MAX_GPU_MESHES: usize = 1024;
const VERTEX_BUFFER_CAPACITY: usize = 1024 * 1024 * 128;

pub struct VickiRenderClient {
    device: Arc<device::Device>,
    raster_simple_render_pass: Arc<RenderPass>,
    accum_img: TemporalImage,
    pub reset_reference_accumulation: bool,
    //cube_index_buffer: Arc<Buffer>,
    meshes: Vec<UploadedTriMesh>,
    mesh_blas: Vec<RayTracingAcceleration>,
    tlas: Option<Arc<RayTracingAcceleration>>,
    mesh_buffer: Mutex<Arc<Buffer>>,
    vertex_buffer: Mutex<Arc<Buffer>>,
    vertex_buffer_written: usize,
    bindless_descriptor_set: vk::DescriptorSet,
    bindless_images: Vec<Image>,
    image_luts: Vec<ImageLut>,
    next_bindless_image_id: usize,
    pub render_mode: RenderMode,
    frame_idx: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Standard,
    Reference,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BindlessImageHandle(pub u32);

/*fn as_byte_slice_unchecked<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * size_of::<T>()) }
}*/

fn append_buffer_data<T: Copy>(buf_slice: &mut [u8], buf_written: &mut usize, data: &[T]) -> usize {
    if !data.is_empty() {
        let alignment = std::mem::align_of::<T>();
        assert!(alignment.count_ones() == 1);

        let data_start = (*buf_written + alignment - 1) & !(alignment - 1);
        let data_bytes = data.len() * size_of::<T>();
        assert!(data_start + data_bytes <= buf_slice.len());

        let dst = unsafe {
            std::slice::from_raw_parts_mut(buf_slice.as_ptr().add(data_start) as *mut T, data.len())
        };
        dst.copy_from_slice(data);

        *buf_written = data_start + data_bytes;
        data_start
    } else {
        0
    }
}

fn create_bindless_descriptor_set(device: &device::Device) -> vk::DescriptorSet {
    let raw_device = &device.raw;

    let set_binding_flags = [
        vk::DescriptorBindingFlags::PARTIALLY_BOUND,
        vk::DescriptorBindingFlags::PARTIALLY_BOUND,
        vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
            | vk::DescriptorBindingFlags::UPDATE_UNUSED_WHILE_PENDING
            | vk::DescriptorBindingFlags::PARTIALLY_BOUND
            | vk::DescriptorBindingFlags::VARIABLE_DESCRIPTOR_COUNT,
    ];

    let mut binding_flags_create_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::builder()
        .binding_flags(&set_binding_flags)
        .build();

    let descriptor_set_layout = unsafe {
        raw_device
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::builder()
                    .bindings(&[
                        vk::DescriptorSetLayoutBinding::builder()
                            .binding(0)
                            .descriptor_count(1)
                            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                            .stage_flags(vk::ShaderStageFlags::ALL)
                            .build(),
                        vk::DescriptorSetLayoutBinding::builder()
                            .binding(1)
                            .descriptor_count(1)
                            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                            .stage_flags(vk::ShaderStageFlags::ALL)
                            .build(),
                        vk::DescriptorSetLayoutBinding::builder()
                            .binding(2)
                            .descriptor_count(MAX_BINDLESS_DESCRIPTOR_COUNT as _)
                            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                            .stage_flags(vk::ShaderStageFlags::ALL)
                            .build(),
                    ])
                    .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                    .push_next(&mut binding_flags_create_info)
                    .build(),
                None,
            )
            .unwrap()
    };

    let descriptor_sizes = [
        vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: 2,
        },
        vk::DescriptorPoolSize {
            ty: vk::DescriptorType::SAMPLED_IMAGE,
            descriptor_count: MAX_BINDLESS_DESCRIPTOR_COUNT as _,
        },
    ];

    let descriptor_pool_info = vk::DescriptorPoolCreateInfo::builder()
        .pool_sizes(&descriptor_sizes)
        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
        .max_sets(1);

    let descriptor_pool = unsafe {
        raw_device
            .create_descriptor_pool(&descriptor_pool_info, None)
            .unwrap()
    };

    let variable_descriptor_count = MAX_BINDLESS_DESCRIPTOR_COUNT as _;
    let mut variable_descriptor_count_allocate_info =
        vk::DescriptorSetVariableDescriptorCountAllocateInfo::builder()
            .descriptor_counts(std::slice::from_ref(&variable_descriptor_count))
            .build();

    let set = unsafe {
        raw_device
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::builder()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(std::slice::from_ref(&descriptor_set_layout))
                    .push_next(&mut variable_descriptor_count_allocate_info)
                    .build(),
            )
            .unwrap()[0]
    };

    set
}

struct BufferBuilder<'a> {
    buf_slice: &'a mut [u8],
    buf_written: &'a mut usize,
}

impl<'a> BufferBuilder<'a> {
    fn new(buf_slice: &'a mut [u8], buf_size: &'a mut usize) -> Self {
        Self {
            buf_slice,
            buf_written: buf_size,
        }
    }

    fn append<T: Copy>(&mut self, data: &[T]) -> usize {
        append_buffer_data(self.buf_slice, &mut self.buf_written, data)
    }
}

impl VickiRenderClient {
    pub fn new(backend: &RenderBackend) -> anyhow::Result<Self> {
        let raster_simple_render_pass = create_render_pass(
            &*backend.device,
            RenderPassDesc {
                color_attachments: &[RenderPassAttachmentDesc::new(
                    vk::Format::R32G32B32A32_SFLOAT,
                )
                .garbage_input()],
                depth_attachment: Some(RenderPassAttachmentDesc::new(
                    vk::Format::D24_UNORM_S8_UINT,
                )),
            },
        )?;

        let mesh_buffer = backend
            .device
            .create_buffer(
                BufferDesc {
                    size: MAX_GPU_MESHES * size_of::<GpuMesh>(),
                    usage: vk::BufferUsageFlags::STORAGE_BUFFER,
                    mapped: true,
                },
                None,
            )
            .unwrap();

        let vertex_buffer = backend
            .device
            .create_buffer(
                BufferDesc {
                    size: VERTEX_BUFFER_CAPACITY,
                    usage: vk::BufferUsageFlags::STORAGE_BUFFER
                        | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                        | vk::BufferUsageFlags::INDEX_BUFFER,
                    mapped: true,
                },
                None,
            )
            .unwrap();

        let bindless_descriptor_set = create_bindless_descriptor_set(backend.device.as_ref());

        Self::write_descriptor_set_buffer(
            &backend.device.raw,
            bindless_descriptor_set,
            0,
            &mesh_buffer,
        );

        Self::write_descriptor_set_buffer(
            &backend.device.raw,
            bindless_descriptor_set,
            1,
            &vertex_buffer,
        );

        let accum_img = backend
            .device
            .create_image(
                ImageDesc::new_2d(vk::Format::R32G32B32A32_SFLOAT, [1280, 720]).usage(
                    vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::STORAGE
                        | vk::ImageUsageFlags::TRANSFER_DST,
                ),
                None,
            )
            .unwrap();

        Ok(Self {
            raster_simple_render_pass,

            accum_img: TemporalImage::new(Arc::new(accum_img)),
            reset_reference_accumulation: false,
            //cube_index_buffer: Arc::new(cube_index_buffer),
            device: backend.device.clone(),
            meshes: Default::default(),
            mesh_blas: Default::default(),
            tlas: Default::default(),
            mesh_buffer: Mutex::new(Arc::new(mesh_buffer)),
            vertex_buffer: Mutex::new(Arc::new(vertex_buffer)),
            vertex_buffer_written: 0,
            bindless_descriptor_set,
            bindless_images: Default::default(),
            image_luts: Default::default(),
            next_bindless_image_id: 0,
            render_mode: RenderMode::Standard,
            frame_idx: 0u32,
        })
    }

    fn write_descriptor_set_buffer(
        device: &slingshot::ash::Device,
        set: vk::DescriptorSet,
        dst_binding: u32,
        buffer: &Buffer,
    ) {
        let buffer_info = vk::DescriptorBufferInfo::builder()
            .buffer(buffer.raw)
            .range(vk::WHOLE_SIZE)
            .build();

        let write_descriptor_set = vk::WriteDescriptorSet::builder()
            .dst_set(set)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .dst_binding(dst_binding)
            .buffer_info(std::slice::from_ref(&buffer_info))
            .build();

        unsafe {
            device.update_descriptor_sets(std::slice::from_ref(&write_descriptor_set), &[]);
        }
    }

    fn add_bindless_image_view(&mut self, view: ImageView) -> BindlessImageHandle {
        let handle = BindlessImageHandle(self.next_bindless_image_id as _);
        self.next_bindless_image_id += 1;

        let image_info = vk::DescriptorImageInfo::builder()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(view)
            .build();

        let write_descriptor_set = vk::WriteDescriptorSet::builder()
            .dst_set(self.bindless_descriptor_set)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .dst_binding(2)
            .dst_array_element(handle.0 as _)
            .image_info(std::slice::from_ref(&image_info))
            .build();

        unsafe {
            self.device
                .raw
                .update_descriptor_sets(std::slice::from_ref(&write_descriptor_set), &[]);
        }

        handle
    }

    pub fn add_image_lut(&mut self, computer: impl ComputeImageLut + 'static, id: usize) {
        self.image_luts
            .push(ImageLut::new(self.device.as_ref(), Box::new(computer)));

        let handle = self.add_bindless_image_view(
            self.image_luts
                .last()
                .unwrap()
                .image
                .view(self.device.as_ref(), &ImageViewDesc::default()),
        );

        assert_eq!(handle.0 as usize, id);
    }

    pub fn add_image(&mut self, src: &RawRgba8Image, params: TexParams) -> BindlessImageHandle {
        let format = match params.gamma {
            crate::asset::mesh::TexGamma::Linear => vk::Format::R8G8B8A8_UNORM,
            crate::asset::mesh::TexGamma::Srgb => vk::Format::R8G8B8A8_SRGB,
        };

        let image = self
            .device
            .create_image(
                ImageDesc::new_2d(format, src.dimensions).usage(vk::ImageUsageFlags::SAMPLED),
                Some(ImageSubResourceData {
                    data: &src.data,
                    row_pitch: src.dimensions[0] as usize * 4,
                    slice_pitch: 0,
                }),
            )
            .unwrap();

        let handle = self
            .add_bindless_image_view(image.view(self.device.as_ref(), &ImageViewDesc::default()));
        self.bindless_images.push(image);
        handle
    }

    pub fn add_mesh(&mut self, mesh: PackedTriangleMesh) {
        let mesh_idx = self.meshes.len();

        let mut vertex_buffer = self.vertex_buffer.lock();
        let mut buffer_builder = BufferBuilder::new(
            Arc::get_mut(&mut *vertex_buffer)
                .expect("refs may not be retained")
                .allocation
                .mapped_slice_mut()
                .expect("vertex buffer pointer"),
            &mut self.vertex_buffer_written,
        );

        let vertex_index_offset = buffer_builder.append(&mesh.indices) as _;
        let vertex_core_offset = buffer_builder.append(&mesh.verts) as _;
        let vertex_uv_offset = buffer_builder.append(&mesh.uvs) as _;
        let vertex_mat_offset = buffer_builder.append(&mesh.material_ids) as _;
        let vertex_aux_offset = buffer_builder.append(&mesh.colors) as _;
        let mat_data_offset = buffer_builder.append(&mesh.materials) as _;

        let mesh_buffer_dst = unsafe {
            let mut mesh_buffer = self.mesh_buffer.lock();
            let mesh_buffer = Arc::get_mut(&mut *mesh_buffer).expect("refs may not be retained");
            let mesh_buffer_dst =
                mesh_buffer.allocation.mapped_ptr().unwrap().as_ptr() as *mut GpuMesh;
            std::slice::from_raw_parts_mut(mesh_buffer_dst, MAX_GPU_MESHES)
        };

        let base_da = vertex_buffer.device_address(&self.device);
        let vertex_buffer_da = base_da + vertex_core_offset as u64;
        let index_buffer_da = base_da + vertex_index_offset as u64;

        let blas = self
            .device
            .create_ray_tracing_bottom_acceleration(&RayTracingBottomAccelerationDesc {
                geometries: vec![RayTracingGeometryDesc {
                    geometry_type: RayTracingGeometryType::Triangle,
                    vertex_buffer: vertex_buffer_da,
                    index_buffer: index_buffer_da,
                    vertex_format: vk::Format::R32G32B32_SFLOAT,
                    vertex_stride: size_of::<PackedVertex>(),
                    parts: vec![RayTracingGeometryPart {
                        index_count: mesh.indices.len(),
                        index_offset: 0,
                        max_vertex: mesh
                            .indices
                            .iter()
                            .copied()
                            .max()
                            .expect("mesh must not be empty"),
                    }],
                }],
            })
            .expect("blas");

        mesh_buffer_dst[mesh_idx] = GpuMesh {
            vertex_core_offset,
            vertex_uv_offset,
            vertex_mat_offset,
            vertex_aux_offset,
            mat_data_offset,
            index_offset: vertex_index_offset,
        };

        self.meshes.push(UploadedTriMesh {
            index_buffer_offset: vertex_index_offset as u64,
            index_count: mesh.indices.len() as _,
        });

        self.mesh_blas.push(blas);
    }

    pub fn build_ray_tracing_top_level_acceleration(&mut self) {
        let tlas = self
            .device
            .create_ray_tracing_top_acceleration(&RayTracingTopAccelerationDesc {
                instances: self.mesh_blas.iter().collect::<Vec<_>>(),
            })
            .expect("tlas");

        self.tlas = Some(Arc::new(tlas));
    }

    pub fn reset_frame_idx(&mut self) {
        self.frame_idx = 0;
    }
}

impl VickiRenderClient {
    fn prepare_render_graph_standard(
        &mut self,
        rg: &mut crate::rg::RenderGraph,
        frame_state: &FrameState,
    ) -> rg::ExportedHandle<Image> {
        let mut depth_img = crate::render_passes::create_image(
            rg,
            ImageDesc::new_2d(vk::Format::D24_UNORM_S8_UINT, frame_state.window_cfg.dims()),
        );
        crate::render_passes::clear_depth(rg, &mut depth_img);

        let mut gbuffer = crate::render_passes::create_image(
            rg,
            ImageDesc::new_2d(
                vk::Format::R32G32B32A32_SFLOAT,
                frame_state.window_cfg.dims(),
            ),
        );
        crate::render_passes::clear_color(rg, &mut gbuffer, [0.0, 0.0, 0.0, 0.0]);

        crate::render_passes::raster_meshes(
            rg,
            self.raster_simple_render_pass.clone(),
            &mut depth_img,
            &mut gbuffer,
            RasterMeshesData {
                meshes: self.meshes.as_slice(),
                vertex_buffer: self.vertex_buffer.lock().clone(),
                bindless_descriptor_set: self.bindless_descriptor_set,
            },
        );

        let tlas = rg.import_ray_tracing_acceleration(
            self.tlas.as_ref().unwrap().clone(),
            vk_sync::AccessType::AnyShaderReadOther,
        );
        let sun_shadow_mask = crate::render_passes::trace_sun_shadow_mask(rg, &depth_img, tlas);

        let mut lit = crate::render_passes::create_image(
            rg,
            ImageDesc::new_2d(
                vk::Format::R16G16B16A16_SFLOAT,
                frame_state.window_cfg.dims(),
            ),
        );
        crate::render_passes::clear_color(rg, &mut lit, [0.0, 0.0, 0.0, 0.0]);
        crate::render_passes::light_gbuffer(
            rg,
            &gbuffer,
            &depth_img,
            &sun_shadow_mask,
            &mut lit,
            self.bindless_descriptor_set,
        );

        rg.export_image(
            lit,
            vk_sync::AccessType::AnyShaderReadSampledImageOrUniformTexelBuffer,
        )
    }

    fn prepare_render_graph_reference(
        &mut self,
        rg: &mut crate::rg::RenderGraph,
        _frame_state: &FrameState,
    ) -> rg::ExportedHandle<Image> {
        let mut accum_img =
            rg.import_image(self.accum_img.resource.clone(), self.accum_img.access_type);

        if self.reset_reference_accumulation {
            self.reset_reference_accumulation = false;
            crate::render_passes::clear_color(rg, &mut accum_img, [0.0, 0.0, 0.0, 0.0]);
        }

        let tlas = rg.import_ray_tracing_acceleration(
            self.tlas.as_ref().unwrap().clone(),
            vk_sync::AccessType::AnyShaderReadOther,
        );

        crate::render_passes::reference_path_trace(
            rg,
            &mut accum_img,
            self.bindless_descriptor_set,
            tlas,
        );

        let lit =
            crate::render_passes::normalize_accum(rg, &accum_img, vk::Format::R16G16B16A16_SFLOAT);

        self.accum_img.last_rg_handle =
            Some(rg.export_image(accum_img, vk_sync::AccessType::Nothing));

        rg.export_image(
            lit,
            vk_sync::AccessType::AnyShaderReadSampledImageOrUniformTexelBuffer,
        )
    }
}

lazy_static::lazy_static! {
    static ref BINDLESS_DESCRIPTOR_SET_LAYOUT: HashMap<u32, rspirv_reflect::DescriptorInfo> = [
        (0, rspirv_reflect::DescriptorInfo {
            ty: rspirv_reflect::DescriptorType::STORAGE_BUFFER,
            is_bindless: false,
            name: Default::default(),
        }),
        (1, rspirv_reflect::DescriptorInfo {
            ty: rspirv_reflect::DescriptorType::STORAGE_BUFFER,
            is_bindless: false,
            name: Default::default(),
        }),
        (2, rspirv_reflect::DescriptorInfo {
            ty: rspirv_reflect::DescriptorType::SAMPLED_IMAGE,
            is_bindless: true,
            name: Default::default(),
        }),
    ]
    .iter()
    .cloned()
    .collect();
}

impl RenderClient<FrameState> for VickiRenderClient {
    fn prepare_render_graph(
        &mut self,
        rg: &mut crate::rg::RenderGraph,
        frame_state: &FrameState,
    ) -> rg::ExportedHandle<Image> {
        rg.predefined_descriptor_set_layouts.insert(
            1,
            rg::PredefinedDescriptorSet {
                bindings: BINDLESS_DESCRIPTOR_SET_LAYOUT.clone(),
            },
        );

        for image_lut in self.image_luts.iter_mut() {
            image_lut.compute(rg);
        }

        match self.render_mode {
            RenderMode::Standard => self.prepare_render_graph_standard(rg, frame_state),
            RenderMode::Reference => self.prepare_render_graph_reference(rg, frame_state),
        }
    }

    fn prepare_frame_constants(
        &mut self,
        dynamic_constants: &mut DynamicConstants,
        frame_state: &FrameState,
    ) {
        let width = frame_state.window_cfg.width;
        let height = frame_state.window_cfg.height;

        dynamic_constants.push(FrameConstants {
            view_constants: ViewConstants::builder(frame_state.camera_matrices, width, height)
                .build(),
            mouse: gen_shader_mouse_state(&frame_state),
            frame_idx: self.frame_idx,
        });
    }

    fn retire_render_graph(&mut self, retired_rg: &RetiredRenderGraph) {
        if let Some(handle) = self.accum_img.last_rg_handle.take() {
            self.accum_img.access_type = retired_rg.get_image(handle).1;
        }

        self.frame_idx = self.frame_idx.overflowing_add(1).0;
    }
}

/*// Vertices: bits 0, 1, 2, map to +/- X, Y, Z
fn cube_indices() -> Vec<u32> {
    let mut res = Vec::with_capacity(6 * 2 * 3);

    for (ndim, dim0, dim1) in [(1, 2, 4), (2, 4, 1), (4, 1, 2)].iter().copied() {
        for (nbit, dim0, dim1) in [(0, dim1, dim0), (ndim, dim0, dim1)].iter().copied() {
            res.push(nbit);
            res.push(nbit + dim0);
            res.push(nbit + dim1);

            res.push(nbit + dim1);
            res.push(nbit + dim0);
            res.push(nbit + dim0 + dim1);
        }
    }

    res
}*/

fn gen_shader_mouse_state(frame_state: &FrameState) -> [f32; 4] {
    let pos = frame_state.input.mouse.pos
        / Vec2::new(
            frame_state.window_cfg.width as f32,
            frame_state.window_cfg.height as f32,
        );

    [
        pos.x(),
        pos.y(),
        if (frame_state.input.mouse.button_mask & 1) != 0 {
            1.0
        } else {
            0.0
        },
        if frame_state.input.keys.is_down(VirtualKeyCode::LShift) {
            -1.0
        } else {
            1.0
        },
    ]
}

#[allow(dead_code)]
struct TemporalImage {
    resource: Arc<Image>,
    access_type: vk_sync::AccessType,
    last_rg_handle: Option<rg::ExportedHandle<Image>>,
}

#[allow(dead_code)]
impl TemporalImage {
    pub fn new(resource: Arc<Image>) -> Self {
        Self {
            resource,
            access_type: vk_sync::AccessType::Nothing,
            last_rg_handle: None,
        }
    }
}