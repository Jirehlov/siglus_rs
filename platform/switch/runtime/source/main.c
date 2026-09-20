#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <switch.h>
#include <deko3d.h>
#include <switch/nvidia/address_space.h>
#include <switch/nvidia/map.h>


enum {
    // Three images leave one image available while Ryujinx/the system retains
    // the most recently presented image and the GPU renders another one.
    // Two images made dkQueueAcquireImage particularly prone to starvation.
    FramebufferCount = 3,
    FramebufferWidth = 1280,
    FramebufferHeight = 720,
    CodeMemorySize = 64 * 1024,
    CommandMemorySize = 16 * 1024,
    // The shared Rust renderer owns a 1280x720 RGBA framebuffer plus an f32
    // depth buffer (roughly 7 MiB before loading any scene assets). libnx's
    // small default NRO heap makes Vec allocation abort inside Renderer::new.
    ApplicationHeapSize = 256 * 1024 * 1024,
};

// libnx provides this as a weak 64-bit symbol. Override it for the Rust VM and
// CPU compositor; loading the game's 11 MiB compressed Scene.pck alone peaks
// near 124 MiB before Horizon's graphics/static allocation overhead. deko3d
// GPU allocations remain in their own memory blocks.
uint64_t __nx_heap_size = ApplicationHeapSize;

static DkDevice device;
static DkMemBlock framebuffer_memory;
static DkImage framebuffers[FramebufferCount];
static uint32_t framebuffer_size;
static DkSwapchain swapchain;
static DkMemBlock scene_texture_memory;
static DkImage scene_texture;
static DkMemBlock descriptor_memory;
static DkGpuAddr descriptor_set;
static DkMemBlock code_memory;
static uint32_t code_offset;
static DkShader vertex_shader;
static DkShader fragment_shader;
static DkMemBlock command_memory;
static DkCmdBuf command_buffer;
static DkCmdList bind_framebuffer[FramebufferCount];
static DkCmdList render_commands;
static DkQueue render_queue;
static NWindow* render_window;
// Signalled after each present. Unlike dkQueueWaitIdle, dkFenceWait accepts a
// timeout, so shutdown cannot become permanently stuck behind a compositor
// buffer which Ryujinx/the system has stopped releasing.
static DkFence last_render_fence;
static bool render_fence_pending;
static const uint8_t* pending_rgba;
static uint32_t pending_rgba_generation;
static uint32_t pending_rgba_fingerprint;
static uint32_t pending_width;
static uint32_t pending_height;
static bool pending_wait_vsync = true;
static bool applied_wait_vsync = true;
static bool vsync_override_logged;
static unsigned acquire_image_failures;
static unsigned rendered_frame_count;
static bool render_fence_timeout_logged;

enum {
    // A 20 ms buffer and six queued buffers tolerate decode/GPU stalls of up
    // to roughly 120 ms without audren underflowing. The old 4 × 10 ms queue
    // was audible as periodic crackle whenever Ryujinx stalled a frame.
    AudioBufferFrames = 960,
    AudioBufferCount = 6,
    // audrv memory pools must begin and end on page boundaries. Each active
    // buffer still submits only AudioBufferFrames; this tail makes every slot
    // exactly one 4 KiB page, for a 24 KiB pool.
    AudioBufferStorageFrames = 1024,
};
static AudioDriver audio_driver;
static AudioDriverWaveBuf audio_wavebufs[AudioBufferCount];
static int16_t audio_buffers[AudioBufferCount][AudioBufferStorageFrames * 2]
    __attribute__((aligned(0x1000)));
static bool audio_renderer_initialized;
static bool audio_driver_initialized;

/* Rust owns the existing SiglusHost/SceneVm.  libnx owns the process and
 * presentation lifetime; no desktop event loop or WGPU object is involved. */
extern void* siglus_switch_engine_create(const char* project_dir, uint32_t width, uint32_t height);
extern bool siglus_switch_engine_step(void* host, uint32_t dt_ms);
extern void siglus_switch_engine_gamepad(void* host, uint8_t button, bool down);
extern void siglus_switch_engine_touch(void* host, int32_t phase, double x, double y);
extern void siglus_switch_engine_destroy(void* host);
extern void siglus_switch_audio_render_i16(int16_t* dst, size_t frames);

/* A packaged game is self-contained: prefer its RomFS payload.  Keeping the
 * SD-card location as a fallback also preserves the small-NRO deployment
 * workflow for developers and for games that are too large for one file. */
static const char* const RomfsGameRoot = "romfs:/game";
static const char* const SdmcGameRoot = "sdmc:/switch/siglus_rs/game";

/* Keep startup diagnostics usable even when Rust's stdio implementation is
 * unavailable on a particular Horizon loader/emulator. */
void siglus_switch_log_message(const char* message) {
    FILE* const file = fopen("sdmc:/switch/siglus_rs/siglus_switch.log", "ab");
    if (file == NULL) return;
    fputs(message, file);
    fclose(file);
}

static void log_startup_result(const char* operation, Result result) {
    char message[96];
    snprintf(message, sizeof(message), "siglus_switch: %s rc=0x%08x\n",
             operation, (unsigned int) result);
    fputs(message, stderr);
    siglus_switch_log_message(message);
}

/* deko3d's debug build reports the underlying DkResult here instead of
 * collapsing it into Horizon's generic ErrorApplet.  Keep this installed in
 * release builds too: an SD-card log is vastly more actionable than a black
 * loading screen on a homebrew launch. */
static void deko_debug_callback(void* user_data, const char* context,
                                DkResult result, const char* message) {
    (void) user_data;
    char log_message[256];
    snprintf(log_message, sizeof(log_message),
             "siglus_switch: deko3d context=%s result=%u message=%s\n",
             context != NULL ? context : "(none)", (unsigned int) result,
             message != NULL ? message : "(none)");
    fputs(log_message, stderr);
    siglus_switch_log_message(log_message);
}

static void log_graphics_marker(const char* operation, uint32_t size) {
    char message[128];
    snprintf(message, sizeof(message), "siglus_switch: graphics %s size=0x%08x\n",
             operation, (unsigned int) size);
    fputs(message, stderr);
    siglus_switch_log_message(message);
}

static void log_graphics_result(const char* operation, DkResult result) {
    char message[128];
    snprintf(message, sizeof(message), "siglus_switch: graphics %s result=%u\n",
             operation, (unsigned int) result);
    fputs(message, stderr);
    siglus_switch_log_message(message);
}

/* Preserve the Horizon Result that deko3d otherwise compresses to
 * DkResult_Fail.  The wrappers are link-time only; all calls still go to the
 * standard libnx implementation. */
extern Result __real_nvMapCreate(NvMap* map, void* cpu_address, u32 size,
                                 u32 alignment, NvKind kind, bool cached);
extern Result __real_nvAddressSpaceMap(NvAddressSpace* address_space, u32 handle,
                                       bool cached, NvKind kind, iova_t* output);

Result __wrap_nvMapCreate(NvMap* map, void* cpu_address, u32 size, u32 alignment,
                          NvKind kind, bool cached) {
    Result rc = __real_nvMapCreate(map, cpu_address, size, alignment, kind, cached);
    log_startup_result("nvMapCreate", rc);
    return rc;
}

Result __wrap_nvAddressSpaceMap(NvAddressSpace* address_space, u32 handle,
                                bool cached, NvKind kind, iova_t* output) {
    Result rc = __real_nvAddressSpaceMap(address_space, handle, cached, kind, output);
    log_startup_result("nvAddressSpaceMap", rc);
    return rc;
}

static const char* select_game_root(void) {
    FILE* const scene_package = fopen("romfs:/game/Scene.pck", "rb");
    if (scene_package != NULL) {
        fclose(scene_package);
        return RomfsGameRoot;
    }
    return SdmcGameRoot;
}

void siglus_switch_random_fill(void* buffer, size_t length) {
    siglus_switch_log_message("siglus_switch: random-fill begin\n");
    randomGet(buffer, length);
    siglus_switch_log_message("siglus_switch: random-fill complete\n");
}

/* Rust's newlib std build uses these Unix-shaped hooks.  Horizon has neither
 * Linux getrandom(2) nor POSIX sysconf(3), so map them to the equivalent
 * libnx primitives in the native shell. */
long getrandom(void* buffer, size_t length, unsigned int flags) {
    (void) flags;
    siglus_switch_random_fill(buffer, length);
    return (long) length;
}

long sysconf(int name) {
    (void) name;
    return 4096;
}

/* Called by the Rust RenderFrame compositor. The pointer remains valid until
 * the next engine frame; render_frame consumes it before that happens. */
void siglus_switch_present_rgba(const uint8_t* pixels, uint32_t width, uint32_t height,
                                bool wait_vsync) {
    pending_rgba = pixels;
    pending_width = width;
    pending_height = height;
    pending_wait_vsync = wait_vsync;
    pending_rgba_generation++;

    // A compact, deterministic sample lets the SD log distinguish a static VM
    // frame from a presentation path that keeps showing stale texture data.
    // Do not hash all 3.5 MiB every frame: the software compositor already
    // owns that cost. This samples 256 evenly spaced bytes instead.
    uint32_t hash = 2166136261u;
    if (pixels != NULL && width != 0 && height != 0) {
        const size_t bytes = (size_t) width * height * 4;
        const size_t stride = bytes / 256 + 1;
        for (size_t offset = 0; offset < bytes; offset += stride) {
            hash = (hash ^ pixels[offset]) * 16777619u;
        }
    }
    pending_rgba_fingerprint = hash;
}

static void initialize_audio(void) {
    static const AudioRendererConfig config = {
        .output_rate = AudioRendererOutputRate_48kHz,
        // audrv allocates one driver channel per configured voice slot. The
        // Kira bridge submits one stereo voice, so it needs two channels even
        // though we only use voice ID 0.
        .num_voices = 2,
        .num_effects = 0,
        .num_sinks = 1,
        .num_mix_objs = 1,
        .num_mix_buffers = 2,
    };
    const Result audren_rc = audrenInitialize(&config);
    if (R_FAILED(audren_rc)) {
        log_startup_result("audrenInitialize", audren_rc);
        return;
    }
    audio_renderer_initialized = true;
    const Result audrv_rc = audrvCreate(&audio_driver, &config, 2);
    if (R_FAILED(audrv_rc)) {
        log_startup_result("audrvCreate", audrv_rc);
        return;
    }
    audio_driver_initialized = true;
    armDCacheFlush(audio_buffers, sizeof(audio_buffers));
    const int pool = audrvMemPoolAdd(&audio_driver, audio_buffers, sizeof(audio_buffers));
    if (pool < 0) {
        siglus_switch_log_message("siglus_switch: audrvMemPoolAdd failed\n");
        return;
    }
    if (!audrvMemPoolAttach(&audio_driver, pool)) {
        siglus_switch_log_message("siglus_switch: audrvMemPoolAttach failed\n");
        return;
    }
    static const u8 sink_channels[] = { 0, 1 };
    audrvDeviceSinkAdd(&audio_driver, AUDREN_DEFAULT_DEVICE_NAME, 2, sink_channels);
    // Commit the memory pool and output sink before creating a voice. libnx's
    // driver does not expose the attached pool to voice setup until this update.
    const Result initial_update_rc = audrvUpdate(&audio_driver);
    if (R_FAILED(initial_update_rc)) {
        log_startup_result("audrvUpdate(pool)", initial_update_rc);
        return;
    }
    const Result start_rc = audrenStartAudioRenderer();
    if (R_FAILED(start_rc)) {
        log_startup_result("audrenStartAudioRenderer", start_rc);
        return;
    }
    if (!audrvVoiceInit(&audio_driver, 0, 2, PcmFormat_Int16, 48000)) {
        siglus_switch_log_message("siglus_switch: audrvVoiceInit failed\n");
        return;
    }
    audrvVoiceSetDestinationMix(&audio_driver, 0, AUDREN_FINAL_MIX_ID);
    audrvVoiceSetMixFactor(&audio_driver, 0, 1.0f, 0, 0);
    audrvVoiceSetMixFactor(&audio_driver, 0, 1.0f, 1, 1);
    audrvVoiceStart(&audio_driver, 0);
    const Result voice_update_rc = audrvUpdate(&audio_driver);
    if (R_FAILED(voice_update_rc)) {
        log_startup_result("audrvUpdate(voice)", voice_update_rc);
        return;
    }
    siglus_switch_log_message("siglus_switch: audio-ready\n");
}

static void pump_audio(void) {
    if (!audio_driver_initialized) return;
    for (unsigned i = 0; i < AudioBufferCount; ++i) {
        AudioDriverWaveBuf* wavebuf = &audio_wavebufs[i];
        if (wavebuf->state != AudioDriverWaveBufState_Free &&
            wavebuf->state != AudioDriverWaveBufState_Done) continue;
        siglus_switch_audio_render_i16(audio_buffers[i], AudioBufferFrames);
        armDCacheFlush(audio_buffers[i], sizeof(audio_buffers[i]));
        wavebuf->data_raw = audio_buffers[i];
        wavebuf->size = (size_t) AudioBufferFrames * 2 * sizeof(int16_t);
        wavebuf->start_sample_offset = 0;
        wavebuf->end_sample_offset = AudioBufferFrames;
        wavebuf->is_looping = false;
        audrvVoiceAddWaveBuf(&audio_driver, 0, wavebuf);
    }
    audrvUpdate(&audio_driver);
}

static void exit_audio(void) {
    if (audio_driver_initialized) audrvClose(&audio_driver);
    if (audio_renderer_initialized) audrenExit();
}

static void load_shader(DkShader* shader, const char* path) {
    FILE* file = fopen(path, "rb");
    if (file == NULL) {
        diagAbortWithResult(MAKERESULT(Module_Libnx, LibnxError_NotFound));
    }

    fseek(file, 0, SEEK_END);
    const long length = ftell(file);
    rewind(file);
    if (length <= 0 || (uint32_t) length > CodeMemorySize - code_offset) {
        fclose(file);
        diagAbortWithResult(MAKERESULT(Module_Libnx, LibnxError_BadInput));
    }

    const uint32_t offset = code_offset;
    code_offset += ((uint32_t) length + DK_SHADER_CODE_ALIGNMENT - 1) &
                   ~(DK_SHADER_CODE_ALIGNMENT - 1);
    fread((uint8_t*) dkMemBlockGetCpuAddr(code_memory) + offset, (size_t) length, 1, file);
    fclose(file);

    DkShaderMaker maker;
    dkShaderMakerDefaults(&maker, code_memory, offset);
    dkShaderInitialize(shader, &maker);
}

static void initialize_graphics(void) {
    DkDeviceMaker device_maker;
    dkDeviceMakerDefaults(&device_maker);
    device_maker.cbDebug = deko_debug_callback;
    device = dkDeviceCreate(&device_maker);
    log_graphics_marker("device-ready", 1);

    DkImageLayoutMaker layout_maker;
    dkImageLayoutMakerDefaults(&layout_maker, device);
    layout_maker.flags = DkImageFlags_UsageRender | DkImageFlags_UsagePresent |
                         DkImageFlags_HwCompression;
    layout_maker.format = DkImageFormat_RGBA8_Unorm;
    layout_maker.dimensions[0] = FramebufferWidth;
    layout_maker.dimensions[1] = FramebufferHeight;

    DkImageLayout framebuffer_layout;
    dkImageLayoutInitialize(&framebuffer_layout, &layout_maker);
    const uint32_t alignment = dkImageLayoutGetAlignment(&framebuffer_layout);
    framebuffer_size = (dkImageLayoutGetSize(&framebuffer_layout) + alignment - 1) &
                       ~(alignment - 1);
    log_graphics_marker("framebuffer-memory", FramebufferCount * framebuffer_size);

    DkMemBlockMaker memory_maker;
    dkMemBlockMakerDefaults(&memory_maker, device, FramebufferCount * framebuffer_size);
    memory_maker.flags = DkMemBlockFlags_GpuCached | DkMemBlockFlags_Image;
    framebuffer_memory = dkMemBlockCreate(&memory_maker);

    const DkImage* images[FramebufferCount];
    for (unsigned int i = 0; i < FramebufferCount; ++i) {
        dkImageInitialize(&framebuffers[i], &framebuffer_layout, framebuffer_memory,
                          i * framebuffer_size);
        images[i] = &framebuffers[i];
    }

    DkSwapchainMaker swapchain_maker;
    render_window = nwindowGetDefault();
    dkSwapchainMakerDefaults(&swapchain_maker, device, render_window, images,
                             FramebufferCount);
    swapchain = dkSwapchainCreate(&swapchain_maker);
    dkSwapchainSetSwapInterval(swapchain, 1);
    log_graphics_marker("swapchain-ready", 1);

    // The engine compositor writes a linear RGBA8 staging texture.  This is
    // deliberately a *new* maker: reusing the presentable-image maker left
    // deko3d's pitch-layout inputs in an invalid state and produced a zero
    // byte layout on Horizon/Ryujinx.
    DkImageLayoutMaker scene_maker;
    dkImageLayoutMakerDefaults(&scene_maker, device);
    scene_maker.type = DkImageType_2D;
    scene_maker.flags = DkImageFlags_PitchLinear | DkImageFlags_Usage2DEngine;
    scene_maker.format = DkImageFormat_RGBA8_Unorm;
    scene_maker.dimensions[0] = FramebufferWidth;
    scene_maker.dimensions[1] = FramebufferHeight;
    /* deko3d 0.5 does not derive a pitch for a linear image when this is
     * zero (unlike newer releases).  Its layout implementation multiplies
     * pitchStride by the width immediately, making the whole image zero
     * bytes. RGBA8 rows are 5120 bytes and already satisfy its 32-byte
     * pitch-linear alignment rule. */
    scene_maker.pitchStride = FramebufferWidth * 4;
    log_graphics_marker("scene-width", scene_maker.dimensions[0]);
    log_graphics_marker("scene-height", scene_maker.dimensions[1]);
    DkImageLayout scene_layout;
    dkImageLayoutInitialize(&scene_layout, &scene_maker);
    /* deko3d 0.5 reports no extra placement alignment for a pitch-linear
     * image.  Treating that documented zero as an arithmetic alignment made
     * `(size + 0 - 1) & ~(0 - 1)` evaluate to zero, and we subsequently
     * asked nvMap for a zero-byte allocation.  A memory block itself is
     * page-aligned, which is stricter than the 32-byte pitch-linear image
     * requirement, so use the layout alignment when present and the libnx
     * page size otherwise. */
    const uint64_t scene_layout_size = dkImageLayoutGetSize(&scene_layout);
    const uint32_t scene_alignment = dkImageLayoutGetAlignment(&scene_layout);
    const uint32_t scene_memory_alignment = scene_alignment != 0 ? scene_alignment : 0x1000;
    const uint32_t scene_size = (uint32_t) ((scene_layout_size + scene_memory_alignment - 1) &
                                            ~(uint64_t) (scene_memory_alignment - 1));
    log_graphics_marker("scene-layout", (uint32_t) scene_layout_size);
    log_graphics_marker("scene-alignment", scene_alignment);
    log_graphics_marker("scene-memory", scene_size);
    dkMemBlockMakerDefaults(&memory_maker, device, scene_size);
    memory_maker.flags = DkMemBlockFlags_CpuUncached | DkMemBlockFlags_GpuCached |
                         DkMemBlockFlags_Image;
    scene_texture_memory = dkMemBlockCreate(&memory_maker);
    dkImageInitialize(&scene_texture, &scene_layout, scene_texture_memory, 0);
    log_graphics_marker("scene-ready", 1);

    dkMemBlockMakerDefaults(&memory_maker, device, 0x1000);
    memory_maker.flags = DkMemBlockFlags_CpuUncached | DkMemBlockFlags_GpuCached;
    descriptor_memory = dkMemBlockCreate(&memory_maker);
    descriptor_set = dkMemBlockGetGpuAddr(descriptor_memory);
    log_graphics_marker("descriptors-ready", 1);

    dkMemBlockMakerDefaults(&memory_maker, device, CodeMemorySize);
    memory_maker.flags = DkMemBlockFlags_CpuUncached | DkMemBlockFlags_GpuCached |
                         DkMemBlockFlags_Code;
    code_memory = dkMemBlockCreate(&memory_maker);
    code_offset = 0;
    load_shader(&vertex_shader, "romfs:/shaders/siglus_vsh.dksh");
    load_shader(&fragment_shader, "romfs:/shaders/siglus_fsh.dksh");
    log_graphics_marker("shaders-ready", 1);

    dkMemBlockMakerDefaults(&memory_maker, device, CommandMemorySize);
    memory_maker.flags = DkMemBlockFlags_CpuUncached | DkMemBlockFlags_GpuCached;
    command_memory = dkMemBlockCreate(&memory_maker);

    DkCmdBufMaker command_maker;
    dkCmdBufMakerDefaults(&command_maker, device);
    command_buffer = dkCmdBufCreate(&command_maker);
    dkCmdBufAddMemory(command_buffer, command_memory, 0, CommandMemorySize);
    log_graphics_marker("commands-ready", 1);

    struct {
        DkImageDescriptor image;
        DkSamplerDescriptor sampler;
    } descriptors;
    DkImageView scene_view;
    dkImageViewDefaults(&scene_view, &scene_texture);
    dkImageDescriptorInitialize(&descriptors.image, &scene_view, false, false);
    DkSampler scene_sampler;
    dkSamplerDefaults(&scene_sampler);
    scene_sampler.wrapMode[0] = DkWrapMode_ClampToEdge;
    scene_sampler.wrapMode[1] = DkWrapMode_ClampToEdge;
    scene_sampler.minFilter = DkFilter_Linear;
    scene_sampler.magFilter = DkFilter_Linear;
    dkSamplerDescriptorInitialize(&descriptors.sampler, &scene_sampler);
    dkCmdBufPushData(command_buffer, descriptor_set, &descriptors, sizeof(descriptors));
    dkCmdBufBindImageDescriptorSet(command_buffer, descriptor_set, 1);
    dkCmdBufBindSamplerDescriptorSet(command_buffer,
        descriptor_set + sizeof(DkImageDescriptor), 1);

    for (unsigned int i = 0; i < FramebufferCount; ++i) {
        DkImageView view;
        dkImageViewDefaults(&view, &framebuffers[i]);
        dkCmdBufBindRenderTarget(command_buffer, &view, NULL);
        bind_framebuffer[i] = dkCmdBufFinishList(command_buffer);
    }

    const DkViewport viewport = { 0.0f, 0.0f, FramebufferWidth, FramebufferHeight, 0.0f, 1.0f };
    const DkScissor scissor = { 0, 0, FramebufferWidth, FramebufferHeight };
    const DkShader* shaders[] = { &vertex_shader, &fragment_shader };
    DkRasterizerState rasterizer;
    DkColorState color_state;
    DkColorWriteState color_write;
    dkRasterizerStateDefaults(&rasterizer);
    dkColorStateDefaults(&color_state);
    dkColorWriteStateDefaults(&color_write);

    dkCmdBufSetViewports(command_buffer, 0, &viewport, 1);
    dkCmdBufSetScissors(command_buffer, 0, &scissor, 1);
    dkCmdBufBindShaders(command_buffer, DkStageFlag_GraphicsMask, shaders, 2);
    dkCmdBufBindRasterizerState(command_buffer, &rasterizer);
    dkCmdBufBindColorState(command_buffer, &color_state);
    dkCmdBufBindColorWriteState(command_buffer, &color_write);
    dkCmdBufBindTexture(command_buffer, DkStage_Fragment, 0, dkMakeTextureHandle(0, 0));
    dkCmdBufDraw(command_buffer, DkPrimitive_Triangles, 6, 1, 0, 0);
    render_commands = dkCmdBufFinishList(command_buffer);

    DkQueueMaker queue_maker;
    dkQueueMakerDefaults(&queue_maker, device);
    queue_maker.flags = DkQueueFlags_Graphics;
    render_queue = dkQueueCreate(&queue_maker);
    log_graphics_marker("queue-ready", 1);
}

static void render_frame(void) {
    // scene_texture has one backing allocation which is reused every frame.
    // Do not overwrite it until the previous draw has finished sampling it.
    // A bounded wait also prevents a wedged GPU queue from blocking the main
    // loop (and therefore audio/input) forever.
    if (render_fence_pending) {
        // Ryujinx can spend several hundred milliseconds compiling the first
        // GPU pipeline. One second avoids treating that one-off warm-up as a
        // stuck queue, while remaining bounded if the queue truly wedges.
        const DkResult fence_result = dkFenceWait(&last_render_fence, 1000 * 1000 * 1000LL);
        if (fence_result != DkResult_Success) {
            if (!render_fence_timeout_logged) {
                log_graphics_result("render-fence-wait", fence_result);
                render_fence_timeout_logged = true;
            }
            return;
        }
        render_fence_timeout_logged = false;
    }

    // The VM's no-vsync flag controls its original timing semantics, but an
    // interval-0 deko3d swapchain can outrun Ryujinx's compositor and later
    // block forever in dkQueueAcquireImage. Keep the native swapchain FIFO
    // paced; the engine continues to process the flag internally.
    if (!pending_wait_vsync && !vsync_override_logged) {
        siglus_switch_log_message("siglus_switch: graphics forcing FIFO present\n");
        vsync_override_logged = true;
    }
    if (!applied_wait_vsync) {
        dkSwapchainSetSwapInterval(swapchain, 1);
        applied_wait_vsync = true;
    }
    // Keep these markers sparse: opening the SD log every frame would itself
    // perturb compositor timing. If the last marker is acquire-begin, then
    // dkQueueAcquireImage is blocking internally rather than returning a
    // negative slot.
    const unsigned frame_number = ++rendered_frame_count;
    const bool trace_frame = frame_number == 1 || frame_number % 120 == 0;
    if (trace_frame) {
        log_graphics_marker("rgba-generation", pending_rgba_generation);
        log_graphics_marker("rgba-fingerprint", pending_rgba_fingerprint);
        log_graphics_marker("acquire-begin", frame_number);
    }
    // dkQueueAcquireImage makes nwindowDequeueBuffer failure fatal. Ryujinx
    // transiently returns an error while its host surface is resized/focused.
    // Reproduce deko3d's acquire path here so that only that OS error is
    // recoverable while preserving its GPU-side compositor-fence wait.
    int slot = -1;
    DkFence acquire_fence = {0};
    *(uint32_t*) acquire_fence._storage = 2; // deko3d Fence::Status_Waiting
    NvMultiFence* const nv_fence =
        (NvMultiFence*) (void*) (acquire_fence._storage + sizeof(uint32_t));
    const Result acquire_result = nwindowDequeueBuffer(render_window, &slot, nv_fence);
    if (R_FAILED(acquire_result)) {
        if (acquire_image_failures++ == 0) {
            log_startup_result("nwindowDequeueBuffer", acquire_result);
        }
        svcSleepThread(16 * 1000 * 1000);
        return;
    }
    dkQueueWaitFence(render_queue, &acquire_fence);
    if (trace_frame) log_graphics_marker("acquire-complete", (uint32_t) slot);
    if (slot < 0 || slot >= FramebufferCount) {
        // deko3d reports a negative slot when the compositor cannot dequeue a
        // buffer (for example while the emulator surface is being recreated).
        // Do not index bind_framebuffer with that value: the old code turned a
        // recoverable presentation failure into an out-of-bounds GPU submit.
        if (acquire_image_failures++ == 0) {
            log_graphics_marker("acquire-image-failed", (uint32_t) slot);
        }
        svcSleepThread(16 * 1000 * 1000);
        return;
    }
    acquire_image_failures = 0;
    if (pending_rgba != NULL && pending_width == FramebufferWidth &&
        pending_height == FramebufferHeight) {
        memcpy(dkMemBlockGetCpuAddr(scene_texture_memory), pending_rgba,
               (size_t) FramebufferWidth * FramebufferHeight * 4);
        dkMemBlockFlushCpuCache(scene_texture_memory, 0,
                                (size_t) FramebufferWidth * FramebufferHeight * 4);
    }
    dkQueueSubmitCommands(render_queue, bind_framebuffer[slot]);
    dkQueueSubmitCommands(render_queue, render_commands);
    // Record GPU completion before handing the image to the compositor. This
    // fence diagnoses a stuck graphics queue without depending on the display
    // service to release its currently presented image.
    dkQueueSignalFence(render_queue, &last_render_fence, true);
    render_fence_pending = true;
    dkQueuePresentImage(render_queue, swapchain, slot);
}

static void exit_graphics(void) {
    // dkQueueWaitIdle() is unbounded; dkSwapchainDestroy() may also wait for a
    // presented image to be dequeued. In particular, both can wait forever
    // after a compositor/acquire failure in Ryujinx. This NRO never recreates
    // graphics objects in-process, so explicit deko3d destruction has no
    // benefit at process exit. Horizon reclaims the device, queue, swapchain,
    // memory blocks, and their NV handles when main returns.
    //
    // Retain only a short diagnostic fence wait. It is intentionally not used
    // as a condition for calling dkSwapchainDestroy: GPU completion does not
    // mean the compositor has returned the image.
    if (render_fence_pending) {
        siglus_switch_log_message("siglus_switch: graphics exit-fence-wait begin\n");
        const DkResult fence_result = dkFenceWait(&last_render_fence, 250 * 1000 * 1000LL);
        log_graphics_result("exit-fence-wait", fence_result);
    }
    if (dkQueueIsInErrorState(render_queue)) {
        siglus_switch_log_message("siglus_switch: graphics exit-queue-error\n");
    }
    siglus_switch_log_message("siglus_switch: graphics exit-process-reclaim\n");
}

int main(void) {
    /* libnx's default __appInit has already called fsInitialize() and
     * fsdevMountSdmc() before main. Mount the NRO payload only after that
     * startup path has completed, and never hide a mount failure. */
    Result rc = romfsMountSelf("romfs");
    log_startup_result("romfsMountSelf", rc);
    if (R_FAILED(rc)) {
        return 1;
    }

    log_graphics_marker("application-heap", (uint32_t) __nx_heap_size);
    initialize_graphics();
    log_graphics_marker("graphics-initialized", 1);
    initialize_audio();

    siglus_switch_log_message("siglus_switch: engine-create begin\n");
    void* engine = siglus_switch_engine_create(select_game_root(),
                                               FramebufferWidth, FramebufferHeight);
    if (engine == NULL) {
        // A missing/corrupt GameData tree must not look like a hung black
        // screen. Rust has already reported the resource/startup error; leave
        // the applet cleanly so homebrew launchers can surface the failure.
        exit_audio();
        exit_graphics();
        romfsExit();
        return 1;
    }
    siglus_switch_log_message("siglus_switch: engine-create complete\n");

    padConfigureInput(1, HidNpadStyleSet_NpadStandard);
    PadState pad;
    padInitializeDefault(&pad);
    bool touch_active = false;
    double last_touch_x = 0.0;
    double last_touch_y = 0.0;
    bool first_frame = true;
    siglus_switch_log_message("siglus_switch: main-loop enter\n");
    while (appletMainLoop()) {
        padUpdate(&pad);
        const uint64_t buttons_down = padGetButtonsDown(&pad);
        const uint64_t buttons_up = padGetButtonsUp(&pad);
        if (engine != NULL) {
            for (uint8_t button = 0; button < 32; ++button) {
                const uint64_t bit = UINT64_C(1) << button;
                if (buttons_down & bit) siglus_switch_engine_gamepad(engine, button, true);
                if (buttons_up & bit) siglus_switch_engine_gamepad(engine, button, false);
            }
        }
        HidTouchScreenState touch_state;
        const size_t touch_state_count = hidGetTouchScreenStates(&touch_state, 1);
        const bool has_touch = touch_state_count != 0 && touch_state.count > 0;
        if (engine != NULL && has_touch) {
            const HidTouchState* touch = &touch_state.touches[0];
            last_touch_x = (double) touch->x;
            last_touch_y = (double) touch->y;
            siglus_switch_engine_touch(engine, touch_active ? 1 : 0,
                                       last_touch_x, last_touch_y);
        } else if (engine != NULL && touch_active) {
            // Button activation requires mouse-down and mouse-up to hit the
            // same object. Keep the final sampled touch position on release;
            // sending (0, 0) here made taps behave like drags off the button.
            siglus_switch_engine_touch(engine, 2, last_touch_x, last_touch_y);
        }
        touch_active = has_touch;
        if (first_frame) siglus_switch_log_message("siglus_switch: first-frame step begin\n");
        if (engine != NULL && siglus_switch_engine_step(engine, 16)) {
            break;
        }
        if (first_frame) siglus_switch_log_message("siglus_switch: first-frame step complete\n");
        pump_audio();
        if (first_frame) siglus_switch_log_message("siglus_switch: first-frame render begin\n");
        render_frame();
        if (first_frame) {
            siglus_switch_log_message("siglus_switch: first-frame render complete\n");
            first_frame = false;
        }
    }

    siglus_switch_engine_destroy(engine);

    exit_audio();
    exit_graphics();
    romfsExit();
    return 0;
}
