#version 460

// Native Switch replacement for the desktop scene vertex shader.  The scene
// renderer supplies per-draw transforms later; this bootstrap shader keeps a
// fullscreen presentation path independent of winit and wgpu.
const vec4 kPositions[6] = vec4[](
    vec4(-1.0, -1.0, 0.0, 1.0), vec4( 1.0, -1.0, 0.0, 1.0),
    vec4( 1.0,  1.0, 0.0, 1.0), vec4(-1.0, -1.0, 0.0, 1.0),
    vec4( 1.0,  1.0, 0.0, 1.0), vec4(-1.0,  1.0, 0.0, 1.0)
);

const vec2 kUv[6] = vec2[](
    vec2(0.0, 1.0), vec2(1.0, 1.0), vec2(1.0, 0.0),
    vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(0.0, 0.0)
);

layout(location = 0) out vec2 out_uv;

void main() {
    gl_Position = kPositions[gl_VertexID];
    out_uv = kUv[gl_VertexID];
}
