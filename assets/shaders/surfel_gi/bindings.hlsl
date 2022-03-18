#ifndef SURFEL_GI_BINDINGS_HLSL
#define SURFEL_GI_BINDINGS_HLSL

#include "../inc/mesh.hlsl" // for VertexPacked

#define DEFINE_SURFEL_GI_BINDINGS(b0, b1, b2, b3, b4) \
    [[vk::binding(b0)]] ByteAddressBuffer surf_rcache_grid_meta_buf; \
    [[vk::binding(b1)]] RWStructuredBuffer<uint> surf_rcache_entry_cell_buf; \
    [[vk::binding(b2)]] StructuredBuffer<VertexPacked> surf_rcache_spatial_buf; \
    [[vk::binding(b3)]] StructuredBuffer<float4> surf_rcache_irradiance_buf; \
    [[vk::binding(b4)]] RWStructuredBuffer<uint> surf_rcache_life_buf;

#endif  // SURFEL_GI_BINDINGS_HLSL