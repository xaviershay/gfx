
use hal::{buffer, command as com, image, memory, pass, pso, query};
use hal::{IndexCount, IndexType, InstanceCount, VertexCount, VertexOffset, WorkGroupCount};
use hal::format::Aspects;

use std::{cmp, iter, mem, ptr};
use std::borrow::Borrow;
use std::ops::Range;

use winapi::um::d3d12;
use winapi::shared::minwindef::{FALSE, UINT};
use winapi::shared::dxgiformat;

use wio::com::ComPtr;

use {conv, native as n, Backend, CmdSignatures, MAX_VERTEX_BUFFERS};
use root_constants::RootConstant;
use smallvec::SmallVec;

// Fixed size of the root signature.
// Limited by D3D12.
const ROOT_SIGNATURE_SIZE: usize = 64;

const NULL_VERTEX_BUFFER_VIEW: d3d12::D3D12_VERTEX_BUFFER_VIEW =
    d3d12::D3D12_VERTEX_BUFFER_VIEW {
        BufferLocation: 0,
        SizeInBytes: 0,
        StrideInBytes: 0,
    };

fn get_rect(rect: &pso::Rect) -> d3d12::D3D12_RECT {
    d3d12::D3D12_RECT {
        left: rect.x as i32,
        top: rect.y as i32,
        right: (rect.x + rect.w) as i32,
        bottom: (rect.y + rect.h) as i32,
    }
}

fn div(a: u32, b: u32) -> u32 {
    assert_eq!(a % b, 0);
    a / b
}

fn bind_descriptor_sets<'a, T>(
    raw: &ComPtr<d3d12::ID3D12GraphicsCommandList>,
    pipeline: &mut PipelineCache,
    layout: &n::PipelineLayout,
    first_set: usize,
    sets: T,
) where
    T: IntoIterator,
    T::Item: Borrow<n::DescriptorSet>,
{
    let mut sets = sets.into_iter().peekable();
    let (srv_cbv_uav_start, sampler_start) = if let Some(set_0) = sets.peek().map(Borrow::borrow) {
        // Bind descriptor heaps
        unsafe {
            // TODO: Can we bind them always or only once?
            //       Resize while recording?
            let mut heaps = [
                set_0.heap_srv_cbv_uav.as_raw(),
                set_0.heap_samplers.as_raw(),
            ];
            raw.SetDescriptorHeaps(2, heaps.as_mut_ptr())
        }

        (set_0.srv_cbv_uav_gpu_start().ptr, set_0.sampler_gpu_start().ptr)
    } else {
        return
    };

    pipeline.srv_cbv_uav_start = srv_cbv_uav_start;
    pipeline.sampler_start = sampler_start;

    let mut table_id = 0;
    for table in &layout.tables[..first_set] {
        if table.contains(n::SRV_CBV_UAV) {
            table_id += 1;
        }
        if table.contains(n::SAMPLERS) {
            table_id += 1;
        }
    }

    let table_base_offset = layout
        .root_constants
        .iter()
        .fold(0, |sum, c| sum + c.range.end - c.range.start);

    for (set, table) in sets.zip(layout.tables[first_set..].iter()) {
        let set = set.borrow();
        set.first_gpu_view.map(|gpu| {
            assert!(table.contains(n::SRV_CBV_UAV));

            let root_offset = table_id + table_base_offset;
            // Cast is safe as offset **must** be in u32 range. Unable to
            // create heaps with more descriptors.
            let table_offset = (gpu.ptr - srv_cbv_uav_start) as u32;
            pipeline
                .user_data
                .set_srv_cbv_uav_table(root_offset as _, table_offset);

            table_id += 1;
        });
        set.first_gpu_sampler.map(|gpu| {
            assert!(table.contains(n::SAMPLERS));

            let root_offset = table_id + table_base_offset;
            // Cast is safe as offset **must** be in u32 range. Unable to
            // create heaps with more descriptors.
            let table_offset = (gpu.ptr - sampler_start) as u32;
            pipeline
                .user_data
                .set_sampler_table(root_offset as _, table_offset);

            table_id += 1;
        });
    }
}

#[derive(Clone)]
struct AttachmentClear {
    subpass_id: Option<pass::SubpassId>,
    value: Option<com::ClearValueRaw>,
    stencil_value: Option<u32>,
}

#[derive(Clone)]
pub struct RenderPassCache {
    render_pass: n::RenderPass,
    framebuffer: n::Framebuffer,
    target_rect: d3d12::D3D12_RECT,
    attachment_clears: Vec<AttachmentClear>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum OcclusionQuery {
    Binary(UINT),
    Precise(UINT),
}

/// Strongly-typed root signature element
///
/// Could be removed for an unsafer variant to occupy less memory
#[derive(Debug, Copy, Clone)]
enum RootElement {
    /// Root constant in the signature
    Constant(u32),
    /// Descriptor table, storing table offset for the current descriptor heap
    TableSrvCbvUav(u32),
    /// Descriptor table, storing table offset for the current descriptor heap
    TableSampler(u32),
    /// Undefined value, implementation specific
    Undefined,
}

/// Virtual data storage for the current root signature memory.
#[derive(Clone)]
struct UserData {
    data: [RootElement; ROOT_SIGNATURE_SIZE],
    dirty_mask: u64,
}

impl UserData {
    fn new() -> Self {
        UserData {
            data: [RootElement::Undefined; ROOT_SIGNATURE_SIZE],
            dirty_mask: 0,
        }
    }

    /// Update root constant values. Changes are marked as dirty.
    fn set_constants(&mut self, offset: usize, data: &[u32]) {
        assert!(offset + data.len() <= ROOT_SIGNATURE_SIZE);
        // Each root constant occupies one DWORD
        for (i, val) in data.iter().enumerate() {
            self.data[offset+i] = RootElement::Constant(*val);
            self.dirty_mask |= 1u64 << (offset + i);
        }
    }

    /// Update descriptor table. Changes are marked as dirty.
    fn set_srv_cbv_uav_table(&mut self, offset: usize, table_start: u32) {
        assert!(offset < ROOT_SIGNATURE_SIZE);
        // A descriptor table occupies one DWORD
        self.data[offset] = RootElement::TableSrvCbvUav(table_start);
        self.dirty_mask |= 1u64 << offset;
    }

    /// Update descriptor table. Changes are marked as dirty.
    fn set_sampler_table(&mut self, offset: usize, table_start: u32) {
        assert!(offset < ROOT_SIGNATURE_SIZE);
        // A descriptor table occupies one DWORD
        self.data[offset] = RootElement::TableSampler(table_start);
        self.dirty_mask |= 1u64 << offset;
    }

    /// Clear dirty flag.
    fn clear_dirty(&mut self, i: usize) {
        self.dirty_mask &= !(1 << i);
    }
}

#[derive(Clone)]
struct PipelineCache {
    // Bound pipeline and root signature.
    // Changed on bind pipeline calls.
    pipeline: Option<(*mut d3d12::ID3D12PipelineState, *mut d3d12::ID3D12RootSignature)>,
    // Paramter slots of the current root signature.
    num_parameter_slots: usize,
    //
    root_constants: Vec<RootConstant>,
    // Virtualized root signature user data of the shaders
    user_data: UserData,

    // Descriptor heap gpu handle offsets
    srv_cbv_uav_start: u64,
    sampler_start: u64,
}

impl PipelineCache {
    fn new() -> Self {
        PipelineCache {
            pipeline: None,
            num_parameter_slots: 0,
            root_constants: Vec::new(),
            user_data: UserData::new(),
            srv_cbv_uav_start: 0,
            sampler_start: 0,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum BindPoint {
    Compute,
    Graphics,
}

#[derive(Clone)]
struct Copy {
    footprint_offset: u64,
    footprint: image::Extent,
    row_pitch: u32,
    img_subresource: u32,
    img_offset: image::Offset,
    buf_offset: image::Offset,
    copy_extent: image::Extent,
}

#[derive(Clone)]
pub struct CommandBuffer {
    raw: ComPtr<d3d12::ID3D12GraphicsCommandList>,
    allocator: ComPtr<d3d12::ID3D12CommandAllocator>,
    signatures: CmdSignatures,

    // Cache renderpasses for graphics operations
    pass_cache: Option<RenderPassCache>,
    cur_subpass: usize,

    // Cache current graphics root signature and pipeline to minimize rebinding and support two
    // bindpoints.
    gr_pipeline: PipelineCache,
    // Cache current compute root signature and pipeline.
    comp_pipeline: PipelineCache,
    // D3D12 only has one slot for both bindpoints. Need to rebind everything if we want to switch
    // between different bind points (ie. calling draw or dispatch).
    active_bindpoint: BindPoint,

    // Active queries in the command buffer.
    // Queries must begin and end in the same command buffer, which allows us to track them.
    // The query pool type on `begin_query` must differ from all currently active queries.
    // Therefore, only one query per query type can be active at the same time. Binary and precise
    // occlusion queries share one queue type in Vulkan.
    occlusion_query: Option<OcclusionQuery>,
    pipeline_stats_query: Option<UINT>,

    // Cached vertex buffer views to bind.
    // `Stride` values are not known at `bind_vertex_buffers` time because they are only stored
    // inside the pipeline state.
    vertex_buffer_views: [d3d12::D3D12_VERTEX_BUFFER_VIEW; MAX_VERTEX_BUFFERS],

    // Re-using allocation for the image-buffer copies.
    copies: Vec<Copy>,

    // D3D12 only allows setting all viewports or all scissors at once, not partial updates.
    // So we must cache the implied state for these partial updates.
    viewport_cache: SmallVec<[d3d12::D3D12_VIEWPORT; d3d12::D3D12_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as usize]>,
    scissor_cache: SmallVec<[d3d12::D3D12_RECT; d3d12::D3D12_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as usize]>
}

unsafe impl Send for CommandBuffer { }
unsafe impl Sync for CommandBuffer { }

impl CommandBuffer {
    pub(crate) fn new(
        raw: ComPtr<d3d12::ID3D12GraphicsCommandList>,
        allocator: ComPtr<d3d12::ID3D12CommandAllocator>,
        signatures: CmdSignatures,
    ) -> Self {
        CommandBuffer {
            raw,
            allocator,
            signatures,
            pass_cache: None,
            cur_subpass: !0,
            gr_pipeline: PipelineCache::new(),
            comp_pipeline: PipelineCache::new(),
            active_bindpoint: BindPoint::Graphics,
            occlusion_query: None,
            pipeline_stats_query: None,
            vertex_buffer_views: [NULL_VERTEX_BUFFER_VIEW; MAX_VERTEX_BUFFERS],
            copies: Vec::new(),
            viewport_cache: SmallVec::new(),
            scissor_cache: SmallVec::new(),
        }
    }

    pub(crate) unsafe fn as_raw_list(&self) -> *mut d3d12::ID3D12CommandList {
        self.raw.as_raw() as *mut _
    }

    fn reset(&mut self) {
        unsafe { self.raw.Reset(self.allocator.as_raw(), ptr::null_mut()); }
        self.pass_cache = None;
        self.cur_subpass = !0;
        self.gr_pipeline = PipelineCache::new();
        self.comp_pipeline = PipelineCache::new();
        self.active_bindpoint = BindPoint::Graphics;
        self.occlusion_query = None;
        self.pipeline_stats_query = None;
        self.vertex_buffer_views = [NULL_VERTEX_BUFFER_VIEW; MAX_VERTEX_BUFFERS];
    }

    fn insert_subpass_barriers(&self) {
        let state = self.pass_cache.as_ref().unwrap();
        let proto_barriers = match state.render_pass.subpasses.get(self.cur_subpass) {
            Some(subpass) => &subpass.pre_barriers,
            None => &state.render_pass.post_barriers,
        };

        let transition_barriers = proto_barriers
            .iter()
            .map(|barrier| {
                let mut resource_barrier = d3d12::D3D12_RESOURCE_BARRIER {
                    Type: d3d12::D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: barrier.flags,
                    u: unsafe { mem::zeroed() },
                };

                *unsafe { resource_barrier.u.Transition_mut() } = d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: state.framebuffer.attachments[barrier.attachment_id].resource,
                    Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: barrier.states.start,
                    StateAfter: barrier.states.end,
                };

                resource_barrier
            })
            .collect::<Vec<_>>();

        if !transition_barriers.is_empty() {
            unsafe {
                self.raw.clone().ResourceBarrier(
                    transition_barriers.len() as _,
                    transition_barriers.as_ptr(),
                );
            }
        }
    }

    fn bind_targets(&mut self) {
        let state = self.pass_cache.as_ref().unwrap();
        let subpass = &state.render_pass.subpasses[self.cur_subpass];

        // collect render targets
        let color_views = subpass.color_attachments
            .iter()
            .map(|&(id, _)| state.framebuffer.attachments[id].handle_rtv.unwrap())
            .collect::<Vec<_>>();
        let ds_view = match subpass.depth_stencil_attachment {
            Some((id, _)) => state.framebuffer.attachments[id].handle_dsv.as_ref().unwrap() as *const _,
            None => ptr::null(),
        };
        // set render targets
        unsafe {
            self.raw.OMSetRenderTargets(
                color_views.len() as UINT,
                color_views.as_ptr(),
                FALSE,
                ds_view,
            );
        }

        // performs clears for all the attachments first used in this subpass
        for (view, clear) in state.framebuffer.attachments.iter().zip(state.attachment_clears.iter()) {
            if clear.subpass_id != Some(self.cur_subpass) {
                continue;
            }

            if let (Some(handle), Some(cv)) = (view.handle_rtv, clear.value) {
                self.clear_render_target_view(handle, unsafe { cv.color }, &[state.target_rect]);
            }

            if let Some(handle) = view.handle_dsv {
                let depth = clear.value.map(|cv| unsafe { cv.depth_stencil.depth });
                let stencil = clear.stencil_value;

                if depth.is_some() || stencil.is_some() {
                    self.clear_depth_stencil_view(handle, depth, stencil, &[state.target_rect]);
                }
            }
        }
    }

    fn clear_render_target_view(
        &self,
        rtv: d3d12::D3D12_CPU_DESCRIPTOR_HANDLE,
        color: com::ClearColorRaw,
        rects: &[d3d12::D3D12_RECT],
    ) {
        let num_rects = rects.len() as _;
        let rects = if num_rects > 0 {
            rects.as_ptr()
        } else {
            ptr::null()
        };

        unsafe {
            self.raw.clone().ClearRenderTargetView(rtv, &color.float32, num_rects, rects);
        }
    }

    fn clear_depth_stencil_view(
        &self,
        dsv: d3d12::D3D12_CPU_DESCRIPTOR_HANDLE,
        depth: Option<f32>,
        stencil: Option<u32>,
        rects: &[d3d12::D3D12_RECT],
    ) {
        let mut flags = 0;
        if depth.is_some() {
            flags = flags | d3d12::D3D12_CLEAR_FLAG_DEPTH;
        }
        if stencil.is_some() {
            flags = flags | d3d12::D3D12_CLEAR_FLAG_STENCIL;
        }

        let num_rects = rects.len() as _;
        let rects = if num_rects > 0 {
            rects.as_ptr()
        } else {
            ptr::null()
        };

        unsafe {
            self.raw.clone().ClearDepthStencilView(
                dsv,
                flags,
                depth.unwrap_or_default(),
                stencil.unwrap_or_default() as _,
                num_rects,
                rects,
            );
        }
    }

    fn set_graphics_bind_point(&mut self) {
        if self.active_bindpoint != BindPoint::Graphics {
            // Switch to graphics bind point
            let (pipeline, _) = self.gr_pipeline.pipeline.expect("No graphics pipeline bound");
            unsafe { self.raw.SetPipelineState(pipeline); }
            self.active_bindpoint = BindPoint::Graphics;
        }

        let cmd_buffer = &mut self.raw;

        // Bind vertex buffers
        // We currently don't support offsets for vertex buffer binding, therefore,
        // we only need to find out how many vertex buffer we need to bind.
        let num_vbs = self.vertex_buffer_views
            .iter()
            .position(|view| view.SizeInBytes == 0)
            .unwrap_or(MAX_VERTEX_BUFFERS);

        unsafe {
            cmd_buffer.IASetVertexBuffers(
                0,
                num_vbs as _,
                self.vertex_buffer_views.as_ptr(),
            );
        }
        // Flush root signature data
        Self::flush_user_data(
            &mut self.gr_pipeline,
            |slot, data| unsafe {
                cmd_buffer.clone().SetGraphicsRoot32BitConstants(
                    slot,
                    data.len() as _,
                    data.as_ptr() as *const _,
                    0,
                )
            },
            |slot, gpu| unsafe {
                cmd_buffer.clone().SetGraphicsRootDescriptorTable(slot, gpu);
            },
        );
    }

    fn set_compute_bind_point(&mut self) {
        if self.active_bindpoint != BindPoint::Compute {
            // Switch to compute bind point
            assert!(self.comp_pipeline.pipeline.is_some(), "No compute pipeline bound");
            let (pipeline, _) = self.comp_pipeline.pipeline.unwrap();
            unsafe { self.raw.SetPipelineState(pipeline); }
            self.active_bindpoint = BindPoint::Compute;
        }

        let cmd_buffer = &mut self.raw;
        Self::flush_user_data(
            &mut self.comp_pipeline,
            |slot, data| unsafe {
                cmd_buffer.clone().SetComputeRoot32BitConstants(
                    slot,
                    data.len() as _,
                    data.as_ptr() as *const _,
                    0,
                )
            },
            |slot, gpu| unsafe {
                cmd_buffer.clone().SetComputeRootDescriptorTable(slot, gpu);
            },
        );
    }

    fn push_constants(
        user_data: &mut UserData,
        layout: &n::PipelineLayout,
        offset: u32,
        constants: &[u32],
    ) {
        let num = constants.len() as u32;
        for root_constant in &layout.root_constants {
            assert!(root_constant.range.start <= root_constant.range.end);
            if root_constant.range.start >= offset &&
               root_constant.range.start < offset+num
            {
                let start = (root_constant.range.start-offset) as _;
                let end = num.min(root_constant.range.end-offset) as _;
                user_data.set_constants(offset as _, &constants[start..end]);
            }
        }
    }

    fn flush_user_data<F, G>(
        pipeline: &mut PipelineCache,
        mut constants_update: F,
        mut table_update: G,
    ) where
        F: FnMut(u32, &[u32]),
        G: FnMut(u32, d3d12::D3D12_GPU_DESCRIPTOR_HANDLE),
    {
        let user_data = &mut pipeline.user_data;
        if user_data.dirty_mask == 0 {
            return
        }

        let num_root_constant = pipeline.root_constants.len();
        let mut cur_index = 0;
        // TODO: opt: Only set dirty root constants?
        for (i, root_constant) in pipeline.root_constants.iter().enumerate() {
            let num_constants = (root_constant.range.end-root_constant.range.start) as usize;
            let mut data = Vec::new();
            for c in cur_index..cur_index+num_constants {
                data.push(match user_data.data[c] {
                    RootElement::Constant(v) => v,
                    _ => {
                        warn!("Unset or mismatching root constant at index {:?} ({:?})", c, user_data.data[c]);
                        0
                    }
                });
                user_data.clear_dirty(c);
            }
            constants_update(i as _, &data);
            cur_index += num_constants;
        }

        // Flush descriptor tables
        // Index in the user data array where tables are starting
        let table_start = pipeline
            .root_constants
            .iter()
            .fold(0, |sum, c| sum + c.range.end - c.range.start) as usize;

        for i in num_root_constant..pipeline.num_parameter_slots {
            let table_index = i - num_root_constant + table_start;
            if ((user_data.dirty_mask >> table_index) & 1) == 1 {
                let ptr = match user_data.data[table_index] {
                    RootElement::TableSrvCbvUav(offset) =>
                        pipeline.srv_cbv_uav_start + offset as u64,
                    RootElement::TableSampler(offset) =>
                        pipeline.sampler_start + offset as u64,
                    other => {
                        error!("Unexpected user data element in the root signature ({:?})", other);
                        continue
                    }
                };
                let gpu = d3d12::D3D12_GPU_DESCRIPTOR_HANDLE { ptr };
                table_update(i as _, gpu);
                user_data.clear_dirty(table_index);
            }
        }
    }

    fn transition_barrier(transition: d3d12::D3D12_RESOURCE_TRANSITION_BARRIER) ->  d3d12::D3D12_RESOURCE_BARRIER {
        let mut barrier = d3d12::D3D12_RESOURCE_BARRIER {
            Type: d3d12::D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: d3d12::D3D12_RESOURCE_BARRIER_FLAG_NONE,
            u: unsafe { mem::zeroed() },
        };

        *unsafe { barrier.u.Transition_mut() } = transition;
        barrier
    }

    fn split_buffer_copy(
        copies: &mut Vec<Copy>, r: &com::BufferImageCopy, image: &n::Image
    ) {
        let buffer_width = if r.buffer_width == 0 {
            r.image_extent.width
        } else {
            r.buffer_width
        };
        let buffer_height = if r.buffer_height == 0 {
            r.image_extent.height
        } else {
            r.buffer_height
        };
        let row_pitch = div(buffer_width, image.block_dim.0 as _) * image.bytes_per_block as u32;
        let slice_pitch = div(buffer_height, image.block_dim.1 as _) * row_pitch;
        let is_pitch_aligned = row_pitch % d3d12::D3D12_TEXTURE_DATA_PITCH_ALIGNMENT == 0;

        for layer in r.image_layers.layers.clone() {
            let img_subresource = image
                .calc_subresource(r.image_layers.level as _, layer as _, 0);
            let layer_offset = r.buffer_offset as u64 + (layer as u32 * slice_pitch * r.image_extent.depth) as u64;
            let aligned_offset = layer_offset & !(d3d12::D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64 - 1);
            if layer_offset == aligned_offset && is_pitch_aligned {
                // trivial case: everything is aligned, ready for copying
                copies.push(Copy {
                    footprint_offset: aligned_offset,
                    footprint: r.image_extent,
                    row_pitch,
                    img_subresource,
                    img_offset: r.image_offset,
                    buf_offset: image::Offset::ZERO,
                    copy_extent: r.image_extent,
                });
            } else if is_pitch_aligned {
                // buffer offset is not aligned
                assert_eq!(image.block_dim, (1, 1)); // TODO
                let row_pitch_texels = row_pitch / image.bytes_per_block as u32;
                let gap = (layer_offset - aligned_offset) as i32;
                let buf_offset = image::Offset {
                    x: gap % row_pitch as i32,
                    y: (gap % slice_pitch as i32) / row_pitch as i32,
                    z: gap / slice_pitch as i32,
                };
                let footprint = image::Extent {
                    width: buf_offset.x as u32 + r.image_extent.width,
                    height: buf_offset.y as u32 + r.image_extent.height,
                    depth: buf_offset.z as u32 + r.image_extent.depth,
                };
                if r.image_extent.width + buf_offset.x as u32 <= row_pitch_texels {
                    // we can map it to the aligned one and adjust the offsets accordingly
                    copies.push(Copy {
                        footprint_offset: aligned_offset,
                        footprint,
                        row_pitch,
                        img_subresource,
                        img_offset: r.image_offset,
                        buf_offset,
                        copy_extent: r.image_extent,
                    });
                } else {
                    // split the copy region into 2 that suffice the previous condition
                    assert!(buf_offset.x as u32 <= row_pitch_texels);
                    let half = row_pitch_texels - buf_offset.x as u32;
                    assert!(half <= r.image_extent.width);

                    copies.push(Copy {
                        footprint_offset: aligned_offset,
                        footprint: image::Extent {
                            width: row_pitch_texels,
                            .. footprint
                        },
                        row_pitch,
                        img_subresource,
                        img_offset: r.image_offset,
                        buf_offset,
                        copy_extent: image::Extent {
                            width: half,
                            .. r.image_extent
                        },
                    });
                    copies.push(Copy {
                        footprint_offset: aligned_offset,
                        footprint: image::Extent {
                            width: r.image_extent.width - half,
                            height: footprint.height + 1,
                            depth: footprint.depth,
                        },
                        row_pitch,
                        img_subresource,
                        img_offset: image::Offset {
                            x: r.image_offset.x + half as i32,
                            .. r.image_offset
                        },
                        buf_offset: image::Offset {
                            x: 0,
                            .. buf_offset
                        },
                        copy_extent: image::Extent {
                            width: r.image_extent.width - half,
                            .. r.image_extent
                        },
                    });
                }
            } else {
                // worst case: row by row copy
                assert_eq!(image.block_dim, (1, 1)); // TODO
                for z in 0 .. r.image_extent.depth {
                    for y in 0 .. r.image_extent.height {
                        // an image row starts non-aligned
                        let row_offset = layer_offset +
                            z as u64 * slice_pitch as u64 +
                            y as u64 * row_pitch as u64;
                        let aligned_offset = row_offset & !(d3d12::D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64 - 1);
                        let next_aligned_offset = aligned_offset + d3d12::D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64;
                        let cut_row_texels = (next_aligned_offset - row_offset) / image.bytes_per_block as u64;
                        let cut_width = cmp::min(r.image_extent.width, cut_row_texels as image::Size);
                        let gap_texels = (row_offset - aligned_offset) as image::Size / image.bytes_per_block as image::Size;
                        // this is a conservative row pitch that should be compatible with both copies
                        let max_unaligned_pitch = r.image_extent.width * image.bytes_per_block as u32;
                        let row_pitch = (max_unaligned_pitch | d3d12::D3D12_TEXTURE_DATA_PITCH_ALIGNMENT) + 1;

                        copies.push(Copy {
                            footprint_offset: aligned_offset,
                            footprint: image::Extent {
                                width: cut_width + gap_texels,
                                height: 1,
                                depth: 1,
                            },
                            row_pitch,
                            img_subresource,
                            img_offset: image::Offset {
                                x: r.image_offset.x,
                                y: r.image_offset.y + y as i32,
                                z: r.image_offset.z + z as i32,
                            },
                            buf_offset: image::Offset {
                                x: gap_texels as i32,
                                y: 0,
                                z: 0,
                            },
                            copy_extent: image::Extent {
                                width: cut_width,
                                height: 1,
                                depth: 1,
                            },
                        });

                        // and if it crosses a pitch alignment - we copy the rest separately
                        if cut_width == r.image_extent.width {
                            continue;
                        }
                        let leftover = r.image_extent.width - cut_width;

                        copies.push(Copy {
                            footprint_offset: next_aligned_offset,
                            footprint: image::Extent {
                                width: leftover,
                                height: 1,
                                depth: 1,
                            },
                            row_pitch,
                            img_subresource,
                            img_offset: image::Offset {
                                x: r.image_offset.x + cut_width as i32,
                                y: r.image_offset.y + y as i32,
                                z: r.image_offset.z + z as i32,
                            },
                            buf_offset: image::Offset::ZERO,
                            copy_extent: image::Extent {
                                width: leftover,
                                height: 1,
                                depth: 1,
                            },
                        });
                    }
                }
            }
        }
    }
}

impl com::RawCommandBuffer<Backend> for CommandBuffer {
    fn begin(&mut self, _flags: com::CommandBufferFlags, _info: com::CommandBufferInheritanceInfo<Backend>) {
        // TODO: Implement flags and secondary command buffers (bundles).
        self.reset();
    }

    fn finish(&mut self) {
        unsafe { self.raw.Close(); }
    }

    fn reset(&mut self, _release_resources: bool) {
        self.reset();
    }

    fn begin_render_pass_raw<T>(
        &mut self,
        render_pass: &n::RenderPass,
        framebuffer: &n::Framebuffer,
        target_rect: pso::Rect,
        clear_values: T,
        _first_subpass: com::SubpassContents,
    ) where
        T: IntoIterator,
        T::Item: Borrow<com::ClearValueRaw>,
    {
        assert_eq!(framebuffer.attachments.len(), render_pass.attachments.len());
        // Make sure that no subpass works with Present as intermediate layout.
        // This wouldn't make much sense, and proceeding with this constraint
        // allows the state transitions generated from subpass dependencies
        // to ignore the layouts completely.
        assert!(!render_pass.subpasses.iter().any(|sp| {
            sp.color_attachments
                .iter()
                .chain(sp.depth_stencil_attachment.iter())
                .chain(sp.input_attachments.iter()).
                any(|aref| aref.1 == image::Layout::Present)
        }));

        let mut clear_iter = clear_values.into_iter();
        let attachment_clears = render_pass.attachments
            .iter()
            .enumerate()
            .map(|(i, attachment)| {
                let cv = if attachment.ops.load == pass::AttachmentLoadOp::Clear || attachment.stencil_ops.load == pass::AttachmentLoadOp::Clear {
                    Some(*clear_iter.next().unwrap().borrow())
                } else {
                    None
                };

                AttachmentClear {
                    subpass_id: render_pass.subpasses.iter().position(|sp| sp.is_using(i)),
                    value: if attachment.ops.load == pass::AttachmentLoadOp::Clear {
                        assert!(cv.is_some());
                        cv
                    } else {
                        None
                    },
                    stencil_value: if attachment.stencil_ops.load == pass::AttachmentLoadOp::Clear {
                        Some(unsafe { cv.unwrap().depth_stencil.stencil })
                    } else {
                        None
                    },
                }
            }).collect();

        self.pass_cache = Some(RenderPassCache {
            render_pass: render_pass.clone(),
            framebuffer: framebuffer.clone(),
            target_rect: get_rect(&target_rect),
            attachment_clears,
        });
        self.cur_subpass = 0;
        self.insert_subpass_barriers();
        self.bind_targets();
    }

    fn next_subpass(&mut self, _contents: com::SubpassContents) {
        self.cur_subpass += 1;
        self.insert_subpass_barriers();
        self.bind_targets();
    }

    fn end_render_pass(&mut self) {
        self.cur_subpass = !0;
        self.insert_subpass_barriers();
        self.pass_cache = None;
    }

    fn pipeline_barrier<'a, T>(
        &mut self,
        _stages: Range<pso::PipelineStage>,
        _dependencies: memory::Dependencies,
        barriers: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<memory::Barrier<'a, Backend>>,
    {
        let mut raw_barriers = Vec::new();

        // transition barriers
        for barrier in barriers {
            match *barrier.borrow() {
                memory::Barrier::AllBuffers(_) |
                memory::Barrier::AllImages(_) => {
                    // Aliasing barrier with NULL resource is the closest we can get to
                    // a global memory barrier in Vulkan.
                    // Was suggested by a Microsoft representative as well as some of the IHVs.
                    let mut bar = d3d12::D3D12_RESOURCE_BARRIER {
                        Type: d3d12::D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Flags: d3d12::D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        u: unsafe { mem::zeroed() },
                    };
                    *unsafe { bar.u.UAV_mut() } = d3d12::D3D12_RESOURCE_UAV_BARRIER {
                        pResource: ptr::null_mut(),
                    };
                    raw_barriers.push(bar);
                }
                memory::Barrier::Buffer { ref states, target } => {
                    let state_src = conv::map_buffer_resource_state(states.start);
                    let state_dst = conv::map_buffer_resource_state(states.end);

                    if state_src == state_dst {
                        continue;
                    }

                    let bar = Self::transition_barrier(
                        d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: target.resource,
                            Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                            StateBefore: state_src,
                            StateAfter: state_dst,
                        }
                    );

                    raw_barriers.push(bar);
                }
                memory::Barrier::Image { ref states, target, ref range } => {
                    let _ = range; //TODO: use subresource range
                    let state_src = conv::map_image_resource_state(states.start.0, states.start.1);
                    let state_dst = conv::map_image_resource_state(states.end.0, states.end.1);

                    if state_src == state_dst {
                        continue;
                    }

                    let mut bar = Self::transition_barrier(
                        d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: target.resource,
                            Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                            StateBefore: state_src,
                            StateAfter: state_dst,
                        }
                    );

                    if *range == target.to_subresource_range(range.aspects) {
                        // Only one barrier if it affects the whole image.
                        raw_barriers.push(bar);
                    } else {
                        // Generate barrier for each layer/level combination.
                        for level in range.levels.clone() {
                            for layer in range.layers.clone() {
                                {
                                    let transition_barrier = &mut *unsafe { bar.u.Transition_mut() };
                                    transition_barrier.Subresource = target.calc_subresource(level as _, layer as _, 0);
                                }
                                raw_barriers.push(bar);
                            }
                        }
                    }
                }
            }
        }

        // UAV barriers
        //
        // TODO: Currently always add a global UAV barrier.
        //       WAR only requires an execution barrier but D3D12 seems to need
        //       a UAV barrier for this according to docs. Can we make this better?
        {
            let mut barrier = d3d12::D3D12_RESOURCE_BARRIER {
                Type: d3d12::D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: d3d12::D3D12_RESOURCE_BARRIER_FLAG_NONE,
                u: unsafe { mem::zeroed() },
            };
            *unsafe { barrier.u.UAV_mut() } = d3d12::D3D12_RESOURCE_UAV_BARRIER {
                pResource: ptr::null_mut(),
            };
            raw_barriers.push(barrier);
        }

        // Alias barriers
        //
        // TODO: Optimize, don't always add an alias barrier
        {
            let mut barrier = d3d12::D3D12_RESOURCE_BARRIER {
                Type: d3d12::D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
                Flags: d3d12::D3D12_RESOURCE_BARRIER_FLAG_NONE,
                u: unsafe { mem::zeroed() },
            };
            *unsafe { barrier.u.Aliasing_mut() } = d3d12::D3D12_RESOURCE_ALIASING_BARRIER {
                pResourceBefore: ptr::null_mut(),
                pResourceAfter: ptr::null_mut(),
            };
            raw_barriers.push(barrier);
        }

        unsafe {
            self.raw.ResourceBarrier(
                raw_barriers.len() as _,
                raw_barriers.as_ptr(),
            );
        }
    }

    fn clear_color_image_raw(
        &mut self,
        image: &n::Image,
        _: image::Layout,
        range: image::SubresourceRange,
        value: com::ClearColorRaw,
    ) {
        assert_eq!(range, image.to_subresource_range(Aspects::COLOR));
        let rtv = image.clear_cv.unwrap();
        self.clear_render_target_view(rtv, value, &[]);
    }

    fn clear_depth_stencil_image_raw(
        &mut self,
        image: &n::Image,
        _layout: image::Layout,
        range: image::SubresourceRange,
        value: com::ClearDepthStencilRaw,
    ) {
        assert!((Aspects::DEPTH | Aspects::STENCIL).contains(range.aspects));
        assert_eq!(range, image.to_subresource_range(range.aspects));
        if range.aspects.contains(Aspects::DEPTH) {
            let dsv = image.clear_dv.unwrap();
            self.clear_depth_stencil_view(dsv, Some(value.depth), None, &[]);
        }
        if range.aspects.contains(Aspects::STENCIL) {
            let dsv = image.clear_sv.unwrap();
            self.clear_depth_stencil_view(dsv, None, Some(value.stencil as _), &[]);
        }
    }

    fn clear_attachments<T, U>(&mut self, clears: T, rects: U)
    where
        T: IntoIterator,
        T::Item: Borrow<com::AttachmentClear>,
        U: IntoIterator,
        U::Item: Borrow<pso::Rect>,
    {
        assert!(self.pass_cache.is_some(), "`clear_attachments` can only be called inside a renderpass");
        let rects: SmallVec<[d3d12::D3D12_RECT; 16]> = rects.into_iter().map(|rect| get_rect(rect.borrow())).collect();
        for clear in clears {
            let clear = clear.borrow();
            match *clear {
                com::AttachmentClear::Color(index, cv) => {
                    let rtv = {
                        let pass_cache = self.pass_cache.as_ref().unwrap();
                        let rtv_id = pass_cache
                            .render_pass
                            .subpasses[self.cur_subpass]
                            .color_attachments[index]
                            .0;

                        pass_cache
                            .framebuffer
                            .attachments[rtv_id]
                            .handle_rtv
                            .unwrap()
                    };

                    self.clear_render_target_view(
                        rtv,
                        cv.into(),
                        &rects,
                    );
                }
                _ => unimplemented!(),
            }
        }
    }

    fn resolve_image<T>(
        &mut self,
        src: &n::Image,
        _src_layout: image::Layout,
        dst: &n::Image,
        _dst_layout: image::Layout,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<com::ImageResolve>,
    {
        {
            // Insert barrier for `COPY_DEST` to `RESOLVE_DEST` as we only expose
            // `TRANSFER_WRITE` which is used for all copy commands.
            let transition_barrier = Self::transition_barrier(
                d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: dst.resource,
                    Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, // TODO: only affected ranges
                    StateBefore: d3d12::D3D12_RESOURCE_STATE_COPY_DEST,
                    StateAfter: d3d12::D3D12_RESOURCE_STATE_RESOLVE_DEST,
                }
            );
            unsafe { self.raw.ResourceBarrier(1, &transition_barrier) };
        }

        for region in regions {
            let r = region.borrow();
            for layer in 0 .. r.extent.depth as UINT {
                unsafe {
                    self.raw.ResolveSubresource(
                        src.resource,
                        src.calc_subresource(r.src_subresource.level as UINT, r.src_subresource.layers.start as UINT + layer, 0),
                        dst.resource,
                        dst.calc_subresource(r.dst_subresource.level as UINT, r.dst_subresource.layers.start as UINT + layer, 0),
                        src.dxgi_format,
                    );
                }
            }
        }

        {
            // Insert barrier for back transition from `RESOLVE_DEST` to `COPY_DEST`.
            let transition_barrier = Self::transition_barrier(
                d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: dst.resource,
                    Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, // TODO: only affected ranges
                    StateBefore: d3d12::D3D12_RESOURCE_STATE_RESOLVE_DEST,
                    StateAfter: d3d12::D3D12_RESOURCE_STATE_COPY_DEST,
                }
            );
            unsafe { self.raw.ResourceBarrier(1, &transition_barrier) };
        }
    }

    fn blit_image<T>(
        &mut self,
        _src: &n::Image,
        _src_layout: image::Layout,
        _dst: &n::Image,
        _dst_layout: image::Layout,
        _filter: image::Filter,
        _regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<com::ImageBlit>
    {
        unimplemented!()
    }

    fn bind_index_buffer(&mut self, ibv: buffer::IndexBufferView<Backend>) {
        let format = match ibv.index_type {
            IndexType::U16 => dxgiformat::DXGI_FORMAT_R16_UINT,
            IndexType::U32 => dxgiformat::DXGI_FORMAT_R32_UINT,
        };
        let location = unsafe { (*ibv.buffer.resource).GetGPUVirtualAddress() };

        let mut ibv_raw = d3d12::D3D12_INDEX_BUFFER_VIEW {
            BufferLocation: location + ibv.offset,
            SizeInBytes: ibv.buffer.size_in_bytes - ibv.offset as u32,
            Format: format,
        };
        unsafe {
            self.raw.IASetIndexBuffer(&mut ibv_raw);
        }
    }

    fn bind_vertex_buffers(&mut self, vbs: pso::VertexBufferSet<Backend>) {
        // Only cache the vertex buffer views as we don't know the stride (PSO).
        for (&(buffer, offset), view) in vbs.0.iter().zip(self.vertex_buffer_views.iter_mut()) {
            let base = unsafe { (*buffer.resource).GetGPUVirtualAddress() };
            view.BufferLocation = base + offset as u64;
            view.SizeInBytes = buffer.size_in_bytes - offset as u32;
        }
    }

    fn set_viewports<T>(&mut self, first_viewport: u32, viewports: T)
    where
        T: IntoIterator,
        T::Item: Borrow<pso::Viewport>,
    {
        let viewports = viewports
            .into_iter()
            .map(|viewport| {
                let viewport = viewport.borrow();
                d3d12::D3D12_VIEWPORT {
                    TopLeftX: viewport.rect.x as _,
                    TopLeftY: viewport.rect.y as _,
                    Width: viewport.rect.w as _,
                    Height: viewport.rect.h as _,
                    MinDepth: viewport.depth.start,
                    MaxDepth: viewport.depth.end,
                }
            })
            .enumerate();
        
        for (i, viewport) in viewports {
            if i + first_viewport as usize >= self.viewport_cache.len() {
                self.viewport_cache.push(viewport);
            } else {
                self.viewport_cache[i + first_viewport as usize] = viewport;
            }
        }

        unsafe {
            self.raw.RSSetViewports(
                self.viewport_cache.len() as _,
                self.viewport_cache.as_ptr(),
            );
        }
    }

    fn set_scissors<T>(&mut self, first_scissor: u32, scissors: T)
    where
        T: IntoIterator,
        T::Item: Borrow<pso::Rect>,
    {
        let rects = scissors
            .into_iter()
            .map(|rect| get_rect(rect.borrow()))
            .enumerate();

        for (i, rect) in rects {
            if i + first_scissor as usize >= self.scissor_cache.len() {
                self.scissor_cache.push(rect);
            } else {
                self.scissor_cache[i + first_scissor as usize] = rect;
            }
        }

        unsafe {
            self.raw
                .RSSetScissorRects(self.scissor_cache.len() as _, self.scissor_cache.as_ptr())
        };
    }

    fn set_blend_constants(&mut self, color: pso::ColorValue) {
        unsafe { self.raw.OMSetBlendFactor(&color); }
    }

    fn set_stencil_reference(&mut self, front: pso::StencilValue, back: pso::StencilValue) {
        if front != back {
            error!(
                "Unable to set different stencil ref values for front ({}) and back ({})",
                front,
                back,
            );
        }

        unsafe { self.raw.OMSetStencilRef(front as _); }
    }

    fn bind_graphics_pipeline(&mut self, pipeline: &n::GraphicsPipeline) {
        unsafe {
            match self.gr_pipeline.pipeline {
                Some((_, signature)) if signature == pipeline.signature => {
                    // Same root signature, nothing to do
                },
                _ => {
                    self.raw.SetGraphicsRootSignature(pipeline.signature);
                    self.gr_pipeline.num_parameter_slots = pipeline.num_parameter_slots;
                    self.gr_pipeline.root_constants = pipeline.constants.clone();
                    // All slots need to be rebound internally on signature change.
                    self.gr_pipeline.user_data.dirty_mask = !0;
                }
            }
            self.raw.SetPipelineState(pipeline.raw);
            self.raw.IASetPrimitiveTopology(pipeline.topology);
        };

        self.active_bindpoint = BindPoint::Graphics;
        self.gr_pipeline.pipeline = Some((pipeline.raw, pipeline.signature));

        // Update strides
        for (view, stride) in self.vertex_buffer_views
                                  .iter_mut()
                                  .zip(pipeline.vertex_strides.iter())
        {
            view.StrideInBytes = *stride;
        }

        if let Some(ref vp) = pipeline.baked_states.viewport {
            self.set_viewports(0, iter::once(vp));
        }
        if let Some(ref rect) = pipeline.baked_states.scissor {
            self.set_scissors(0, iter::once(rect));
        }
        if let Some(color) = pipeline.baked_states.blend_color {
            self.set_blend_constants(color);
        }
    }

    fn bind_graphics_descriptor_sets<'a, T>(
        &mut self,
        layout: &n::PipelineLayout,
        first_set: usize,
        sets: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<n::DescriptorSet>,
    {
        bind_descriptor_sets(&self.raw, &mut self.gr_pipeline, layout, first_set, sets);
    }

    fn bind_compute_pipeline(&mut self, pipeline: &n::ComputePipeline) {
        unsafe {
            match self.comp_pipeline.pipeline {
                Some((_, signature)) if signature == pipeline.signature => {
                    // Same root signature, nothing to do
                },
                _ => {
                    self.raw.SetComputeRootSignature(pipeline.signature);
                    self.comp_pipeline.num_parameter_slots = pipeline.num_parameter_slots;
                    self.comp_pipeline.root_constants = pipeline.constants.clone();
                    // All slots need to be rebound internally on signature change.
                    self.comp_pipeline.user_data.dirty_mask = !0;
                }
            }
            self.raw.SetPipelineState(pipeline.raw);
        }

        self.active_bindpoint = BindPoint::Compute;
        self.comp_pipeline.pipeline = Some((pipeline.raw, pipeline.signature));
    }

    fn bind_compute_descriptor_sets<T>(
        &mut self,
        layout: &n::PipelineLayout,
        first_set: usize,
        sets: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<n::DescriptorSet>,
    {
        bind_descriptor_sets(&self.raw, &mut self.comp_pipeline, layout, first_set, sets);
    }

    fn dispatch(&mut self, count: WorkGroupCount) {
        self.set_compute_bind_point();
        unsafe {
            self.raw.Dispatch(count[0], count[1], count[2]);
        }
    }

    fn dispatch_indirect(&mut self, buffer: &n::Buffer, offset: buffer::Offset) {
        self.set_compute_bind_point();
        unsafe {
            self.raw.ExecuteIndirect(
                self.signatures.dispatch.as_raw(),
                1,
                buffer.resource,
                offset,
                ptr::null_mut(),
                0,
            );
        }
    }

    fn fill_buffer(
        &mut self,
        buffer: &n::Buffer,
        range: Range<buffer::Offset>,
        data: u32,
    ) {
        assert!(buffer.clear_uav.is_some(), "Buffer needs to be created with usage `TRANSFER_DST`");
        assert_eq!(range, 0..buffer.size_in_bytes as u64); // TODO: Need to dynamically create UAVs

        // Insert barrier for `COPY_DEST` to `UNORDERED_ACCESS` as we use
        // `TRANSFER_WRITE` for all clear commands.
        let transition_barrier = Self::transition_barrier(
            d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: buffer.resource,
                Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: d3d12::D3D12_RESOURCE_STATE_COPY_DEST,
                StateAfter: d3d12::D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            }
        );
        unsafe { self.raw.ResourceBarrier(1, &transition_barrier) };

        let handles = buffer.clear_uav.unwrap();
        unsafe {
            self.raw.ClearUnorderedAccessViewUint(
                handles.gpu,
                handles.cpu,
                buffer.resource,
                &[data as UINT; 4],
                0,
                ptr::null_mut(), // TODO: lift with the forementioned restriction
            );
        }

        let transition_barrier = Self::transition_barrier(
            d3d12::D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: buffer.resource,
                Subresource: d3d12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: d3d12::D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                StateAfter: d3d12::D3D12_RESOURCE_STATE_COPY_DEST,
            }
        );
        unsafe { self.raw.ResourceBarrier(1, &transition_barrier) };
    }

    fn update_buffer(
        &mut self,
        _buffer: &n::Buffer,
        _offset: buffer::Offset,
        _data: &[u8],
    ) {
        unimplemented!()
    }

    fn copy_buffer<T>(&mut self, src: &n::Buffer, dst: &n::Buffer, regions: T)
    where
        T: IntoIterator,
        T::Item: Borrow<com::BufferCopy>,
    {
        // copy each region
        for region in regions {
            let region = region.borrow();
            unsafe {
                self.raw.CopyBufferRegion(
                    dst.resource,
                    region.dst as _,
                    src.resource,
                    region.src as _,
                    region.size as _,
                );
            }
        }

        // TODO: Optimization: Copy whole resource if possible
    }

    fn copy_image<T>(
        &mut self,
        src: &n::Image,
        _: image::Layout,
        dst: &n::Image,
        _: image::Layout,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<com::ImageCopy>,
    {
        let mut src_image = d3d12::D3D12_TEXTURE_COPY_LOCATION {
            pResource: src.resource,
            Type: d3d12::D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            u: unsafe { mem::zeroed() },
        };

        let mut dst_image = d3d12::D3D12_TEXTURE_COPY_LOCATION {
            pResource: dst.resource,
            Type: d3d12::D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            u: unsafe { mem::zeroed() },
        };

        for region in regions {
            let r = region.borrow();
            debug_assert_eq!(r.src_subresource.layers.len(), r.dst_subresource.layers.len());
            let num_layers = r.src_subresource.layers.len() as image::Layer;
            let src_layer_start = r.src_subresource.layers.start;
            let dst_layer_start = r.dst_subresource.layers.start;
            let src_box = d3d12::D3D12_BOX {
                left: r.src_offset.x as _,
                top: r.src_offset.y as _,
                right: (r.src_offset.x + r.extent.width as i32) as _,
                bottom: (r.src_offset.y + r.extent.height as i32) as _,
                front: r.src_offset.z as _,
                back: (r.src_offset.z + r.extent.depth as i32) as _,
            };

            for layer in 0..num_layers {
                *unsafe { src_image.u.SubresourceIndex_mut() } =
                    src.calc_subresource(r.src_subresource.level as _, (src_layer_start + layer) as _, 0);
                *unsafe { dst_image.u.SubresourceIndex_mut() } =
                    dst.calc_subresource(r.dst_subresource.level as _, (dst_layer_start + layer) as _, 0);
                unsafe {
                    self.raw.CopyTextureRegion(
                        &dst_image,
                        r.dst_offset.x as _,
                        r.dst_offset.y as _,
                        r.dst_offset.z as _,
                        &src_image,
                        &src_box,
                    );
                }
            }
        }
    }

    fn copy_buffer_to_image<T>(
        &mut self,
        buffer: &n::Buffer,
        image: &n::Image,
        _: image::Layout,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<com::BufferImageCopy>,
    {
        assert!(self.copies.is_empty());

        for region in regions {
            let r = region.borrow();
            Self::split_buffer_copy(&mut self.copies, r, image);
        }

        if self.copies.is_empty() {
            return;
        }

        let mut src = d3d12::D3D12_TEXTURE_COPY_LOCATION {
            pResource: buffer.resource,
            Type: d3d12::D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            u: unsafe { mem::zeroed() },
        };
        let mut dst = d3d12::D3D12_TEXTURE_COPY_LOCATION {
            pResource: image.resource,
            Type: d3d12::D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            u: unsafe { mem::zeroed() },
        };

        for c in self.copies.drain(..) {
            let src_box = d3d12::D3D12_BOX {
                left: c.buf_offset.x as u32,
                top: c.buf_offset.y as u32,
                right: c.buf_offset.x as u32 + c.copy_extent.width,
                bottom: c.buf_offset.y as u32 + c.copy_extent.height,
                front: c.buf_offset.z as u32,
                back: c.buf_offset.z as u32 + c.copy_extent.depth,
            };
            let footprint = d3d12::D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: c.footprint_offset,
                Footprint: d3d12::D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: image.dxgi_format,
                    Width: c.footprint.width,
                    Height: c.footprint.height,
                    Depth: c.footprint.depth,
                    RowPitch: c.row_pitch,
                },
            };
            unsafe {
                *src.u.PlacedFootprint_mut() = footprint;
                *dst.u.SubresourceIndex_mut() = c.img_subresource;
                self.raw.CopyTextureRegion(
                    &dst,
                    c.img_offset.x as _,
                    c.img_offset.y as _,
                    c.img_offset.z as _,
                    &src,
                    &src_box,
                );
            }
        }
    }

    fn copy_image_to_buffer<T>(
        &mut self,
        image: &n::Image,
        _: image::Layout,
        buffer: &n::Buffer,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<com::BufferImageCopy>,
    {
        assert!(self.copies.is_empty());

        for region in regions {
            let r = region.borrow();
            Self::split_buffer_copy(&mut self.copies, r, image);
        }

        if self.copies.is_empty() {
            return;
        }

        let mut src = d3d12::D3D12_TEXTURE_COPY_LOCATION {
            pResource: image.resource,
            Type: d3d12::D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            u: unsafe { mem::zeroed() },
        };
        let mut dst = d3d12::D3D12_TEXTURE_COPY_LOCATION {
            pResource: buffer.resource,
            Type: d3d12::D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            u: unsafe { mem::zeroed() },
        };

        for c in self.copies.drain(..) {
            let src_box = d3d12::D3D12_BOX {
                left: c.img_offset.x as u32,
                top: c.img_offset.y as u32,
                right: c.img_offset.x as u32 + c.copy_extent.width,
                bottom: c.img_offset.y as u32 + c.copy_extent.height,
                front: c.img_offset.z as u32,
                back: c.img_offset.z as u32 + c.copy_extent.depth,
            };
            let footprint = d3d12::D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: c.footprint_offset,
                Footprint: d3d12::D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: image.dxgi_format,
                    Width: c.footprint.width,
                    Height: c.footprint.height,
                    Depth: c.footprint.depth,
                    RowPitch: c.row_pitch,
                },
            };
            unsafe {
                *dst.u.PlacedFootprint_mut() = footprint;
                *src.u.SubresourceIndex_mut() = c.img_subresource;
                self.raw.CopyTextureRegion(
                    &dst,
                    c.buf_offset.x as _,
                    c.buf_offset.y as _,
                    c.buf_offset.z as _,
                    &src,
                    &src_box,
                );
            }
        }
    }

    fn draw(&mut self, vertices: Range<VertexCount>, instances: Range<InstanceCount>) {
        self.set_graphics_bind_point();
        unsafe {
            self.raw.DrawInstanced(
                vertices.end - vertices.start,
                instances.end - instances.start,
                vertices.start,
                instances.start,
            );
        }
    }

    fn draw_indexed(
        &mut self,
        indices: Range<IndexCount>,
        base_vertex: VertexOffset,
        instances: Range<InstanceCount>,
    ) {
        self.set_graphics_bind_point();
        unsafe {
            self.raw.DrawIndexedInstanced(
                indices.end - indices.start,
                instances.end - instances.start,
                indices.start,
                base_vertex,
                instances.start,
            );
        }
    }

    fn draw_indirect(
        &mut self,
        buffer: &n::Buffer,
        offset: buffer::Offset,
        draw_count: u32,
        stride: u32,
    ) {
        assert_eq!(stride, 16);
        self.set_graphics_bind_point();
        unsafe {
            self.raw.ExecuteIndirect(
                self.signatures.draw.as_raw(),
                draw_count,
                buffer.resource,
                offset,
                ptr::null_mut(),
                0,
            );
        }
    }

    fn draw_indexed_indirect(
        &mut self,
        buffer: &n::Buffer,
        offset: buffer::Offset,
        draw_count: u32,
        stride: u32,
    ) {
        assert_eq!(stride, 20);
        self.set_graphics_bind_point();
        unsafe {
            self.raw.ExecuteIndirect(
                self.signatures.draw_indexed.as_raw(),
                draw_count,
                buffer.resource,
                offset,
                ptr::null_mut(),
                0,
            );
        }
    }

    fn begin_query(
        &mut self,
        query: query::Query<Backend>,
        flags: query::QueryControl,
    ) {
        let query_ty = match query.pool.ty {
            d3d12::D3D12_QUERY_HEAP_TYPE_OCCLUSION => {
                if flags.contains(query::QueryControl::PRECISE) {
                    self.occlusion_query = Some(OcclusionQuery::Precise(query.id));
                    d3d12::D3D12_QUERY_TYPE_OCCLUSION
                } else {
                    // Default to binary occlusion as it might be faster due to early depth/stencil
                    // tests.
                    self.occlusion_query = Some(OcclusionQuery::Binary(query.id));
                    d3d12::D3D12_QUERY_TYPE_BINARY_OCCLUSION
                }
            }
            d3d12::D3D12_QUERY_HEAP_TYPE_TIMESTAMP => {
                panic!("Timestap queries are issued via ")
            }
            d3d12::D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS => {
                self.pipeline_stats_query = Some(query.id);
                d3d12::D3D12_QUERY_TYPE_PIPELINE_STATISTICS
            }
            _ => unreachable!(),
        };

        unsafe {
            self.raw.BeginQuery(
                query.pool.raw.as_raw(),
                query_ty,
                query.id,
            );
        }
    }

    fn end_query(
        &mut self,
        query: query::Query<Backend>,
    ) {
        let id = query.id;
        let query_ty = match query.pool.ty {
            d3d12::D3D12_QUERY_HEAP_TYPE_OCCLUSION
                if self.occlusion_query == Some(OcclusionQuery::Precise(id)) =>
            {
                self.occlusion_query = None;
                d3d12::D3D12_QUERY_TYPE_OCCLUSION
            }
            d3d12::D3D12_QUERY_HEAP_TYPE_OCCLUSION
                if self.occlusion_query == Some(OcclusionQuery::Binary(id)) =>
            {
                self.occlusion_query = None;
                d3d12::D3D12_QUERY_TYPE_BINARY_OCCLUSION
            }
            d3d12::D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS
                if self.pipeline_stats_query == Some(id) =>
            {
                self.pipeline_stats_query = None;
                d3d12::D3D12_QUERY_TYPE_PIPELINE_STATISTICS
            }
            _ => panic!("Missing `begin_query` call for query: {:?}", query),
        };

        unsafe {
            self.raw.EndQuery(
                query.pool.raw.as_raw(),
                query_ty,
                id,
            );
        }
    }

    fn reset_query_pool(
        &mut self,
        _pool: &n::QueryPool,
        _queries: Range<query::QueryId>,
    ) {
        // Nothing to do here
        // vkCmdResetQueryPool sets the queries to `unavailable` but the specification
        // doesn't state an affect on the `active` state. Every queries at the end of the command
        // buffer must be made inactive, which can only be done with EndQuery.
        // Therefore, every `begin_query` must follow a `end_query` state, the resulting values
        // after calling are undefined.
    }

    fn write_timestamp(
        &mut self,
        _: pso::PipelineStage,
        query: query::Query<Backend>,
    ) {
        unsafe {
            self.raw.EndQuery(
                query.pool.raw.as_raw(),
                d3d12::D3D12_QUERY_TYPE_TIMESTAMP,
                query.id,
            );
        }
    }

    fn push_graphics_constants(
        &mut self,
        layout: &n::PipelineLayout,
        _stages: pso::ShaderStageFlags,
        offset: u32,
        constants: &[u32],
    ) {
        Self::push_constants(&mut self.gr_pipeline.user_data, layout, offset, constants);
    }

    fn push_compute_constants(
        &mut self,
        layout: &n::PipelineLayout,
        offset: u32,
        constants: &[u32],
    ) {
        Self::push_constants(&mut self.comp_pipeline.user_data, layout, offset, constants);
    }

    fn execute_commands<I>(
        &mut self,
        buffers: I,
    ) where
        I: IntoIterator,
        I::Item: Borrow<CommandBuffer>,
    {
        for _cmd_buf in buffers {
            error!("TODO: execute_commands");
        }
    }
}
