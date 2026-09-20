#version 460

// Native Switch presentation shader. The existing engine produces its normal
// painter-ordered RenderFrame; the Horizon backend uploads the resulting RGBA8
// surface and samples it with a deko3d descriptor rather than WGPU.
layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;
layout(binding = 0) uniform sampler2D scene_texture;

void main() {
    out_color = texture(scene_texture, in_uv);
}
