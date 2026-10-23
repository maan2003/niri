/*
 * Diagnostic client for the actual split-process compositor, not a mock.
 * Generate xdg-shell and linux-dmabuf client code with wayland-scanner, then link
 * this file with those sources and pkg-config --cflags --libs wayland-client gbm egl glesv2.
 *
 * Usage: asynchronous-buffer-release RENDER_NODE CLIENT_UID
 * Allocate two linear DMA-bufs before dropping to CLIENT_UID (the compositor
 * must authorize that UID for GPU buffers). Commands on stdin: A, B, repaint-A, quit.
 * Every attach commits full damage and requests a frame callback. Logs distinguish
 * commit roundtrips, frame callbacks, and actual wl_buffer.release events.
 *
 * The separate VM-only harness stops/resumes the GPU worker. This client never
 * sends signals or changes compositor configuration. repaint-A writes cyan ONLY
 * after the actual release listener marks A released since its latest attach.
 */
#undef NDEBUG
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <poll.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "linux-dmabuf-client-protocol.h"

enum { WIDTH = 641, HEIGHT = 479 };
static struct wl_display *display;
static struct wl_compositor *compositor;
static struct zwp_linux_dmabuf_v1 *dmabuf;
static struct xdg_wm_base *wm;
static struct wl_surface *surface;
static unsigned sequence, configured;
static struct timespec start;
struct buffer_info {
    char name;
    bool released;
    unsigned latest_attach;
};

static double elapsed(void)
{
    struct timespec now;
    assert(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
    return now.tv_sec - start.tv_sec + (now.tv_nsec - start.tv_nsec) / 1e9;
}
static void released(void *data, struct wl_buffer *buffer)
{
    (void)buffer;
    struct buffer_info *info = data;
    info->released = true;
    printf("%.6f RELEASE buffer=%c latest_commit=%u latest_attach=%u\n",
           elapsed(), info->name, sequence, info->latest_attach);
}
static const struct wl_buffer_listener buffer_listener = { .release = released };
struct frame_info { unsigned sequence; char buffer; };
static void frame_done(void *data, struct wl_callback *callback, uint32_t time)
{
    struct frame_info *info = data;
    printf("%.6f FRAME buffer=%c commit=%u compositor_time=%u\n",
           elapsed(), info->buffer, info->sequence, time);
    wl_callback_destroy(callback);
    free(info);
}
static const struct wl_callback_listener frame_listener = { .done = frame_done };
static void ping(void *data, struct xdg_wm_base *base, uint32_t serial)
{
    (void)data;
    xdg_wm_base_pong(base, serial);
}
static const struct xdg_wm_base_listener wm_listener = { .ping = ping };
static void configure(void *data, struct xdg_surface *xdg, uint32_t serial)
{
    (void)data;
    xdg_surface_ack_configure(xdg, serial);
    configured = 1;
}
static const struct xdg_surface_listener xdg_listener = { .configure = configure };
static void toplevel_configure(void *d, struct xdg_toplevel *t, int32_t w,
                              int32_t h, struct wl_array *states)
{
    (void)d; (void)t; (void)w; (void)h; (void)states;
}
static void toplevel_close(void *d, struct xdg_toplevel *t)
{
    (void)d; (void)t;
    fprintf(stderr, "unexpected toplevel close\n");
    exit(1);
}
static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure, .close = toplevel_close,
};
static void global(void *d, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version)
{
    (void)d; (void)version;
    if (!strcmp(interface, "wl_compositor"))
        compositor = wl_registry_bind(registry, name, &wl_compositor_interface, 4);
    if (!strcmp(interface, "xdg_wm_base")) {
        wm = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wm, &wm_listener, NULL);
    }
    if (!strcmp(interface, "zwp_linux_dmabuf_v1"))
        dmabuf = wl_registry_bind(registry, name, &zwp_linux_dmabuf_v1_interface, 3);
}
static void global_remove(void *d, struct wl_registry *r, uint32_t name)
{
    (void)d; (void)r; (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = global, .global_remove = global_remove,
};
static void paint(struct gbm_bo *bo, EGLDisplay egl, uint32_t color)
{
    int fd = gbm_bo_get_fd(bo);
    assert(fd >= 0);
    EGLint attrs[] = {
        EGL_WIDTH, WIDTH, EGL_HEIGHT, HEIGHT,
        EGL_LINUX_DRM_FOURCC_EXT, GBM_FORMAT_XRGB8888,
        EGL_DMA_BUF_PLANE0_FD_EXT, fd,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, gbm_bo_get_stride(bo),
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, 0,
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, 0, EGL_NONE,
    };
    PFNEGLCREATEIMAGEKHRPROC create_image =
        (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLDESTROYIMAGEKHRPROC destroy_image =
        (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    assert(create_image && destroy_image && image_target);
    EGLImageKHR image = create_image(egl, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    assert(image != EGL_NO_IMAGE_KHR);
    GLuint texture, framebuffer;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    image_target(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    assert(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
    glClearColor(((color >> 16) & 255) / 255.f, ((color >> 8) & 255) / 255.f,
                 (color & 255) / 255.f, 1.f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish(); // Complete this producer write before reporting it to the harness.
    assert(glGetError() == GL_NO_ERROR);
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteTextures(1, &texture);
    assert(destroy_image(egl, image));
    close(fd);
}
static struct gbm_bo *allocate(struct gbm_device *device, EGLDisplay egl, uint32_t color)
{
    const uint64_t linear = 0;
    struct gbm_bo *bo = gbm_bo_create_with_modifiers2(
        device, WIDTH, HEIGHT, GBM_FORMAT_XRGB8888, &linear, 1,
        GBM_BO_USE_RENDERING);
    assert(bo && gbm_bo_get_modifier(bo) == linear);
    paint(bo, egl, color);
    return bo;
}
static struct wl_buffer *import(struct gbm_bo *bo, struct buffer_info *info)
{
    int fd = gbm_bo_get_fd(bo);
    assert(fd >= 0);
    struct zwp_linux_buffer_params_v1 *params =
        zwp_linux_dmabuf_v1_create_params(dmabuf);
    zwp_linux_buffer_params_v1_add(params, fd, 0, 0, gbm_bo_get_stride(bo), 0, 0);
    struct wl_buffer *buffer = zwp_linux_buffer_params_v1_create_immed(
        params, WIDTH, HEIGHT, GBM_FORMAT_XRGB8888, 0);
    zwp_linux_buffer_params_v1_destroy(params);
    close(fd);
    assert(wl_buffer_add_listener(buffer, &buffer_listener, info) == 0);
    return buffer;
}
int main(int argc, char **argv)
{
    assert(argc == 3);
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stdin, NULL, _IONBF, 0);
    assert(clock_gettime(CLOCK_MONOTONIC, &start) == 0);
    int fd = open(argv[1], O_RDWR | O_CLOEXEC);
    assert(fd >= 0);
    struct gbm_device *device = gbm_create_device(fd);
    assert(device);
    EGLDisplay egl = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, device, NULL);
    assert(egl != EGL_NO_DISPLAY && eglInitialize(egl, NULL, NULL));
    assert(eglBindAPI(EGL_OPENGL_ES_API));
    const EGLint attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext context = eglCreateContext(egl, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attrs);
    assert(context != EGL_NO_CONTEXT);
    assert(eglMakeCurrent(egl, EGL_NO_SURFACE, EGL_NO_SURFACE, context));
    fprintf(stderr, "Producer: %s; %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));
    struct gbm_bo *bos[2] = {
        allocate(device, egl, 0xff27b35b), allocate(device, egl, 0xffd34397),
    };
    uid_t uid = (uid_t)strtoul(argv[2], NULL, 10);
    assert(uid > 0 && setgid(uid) == 0 && setuid(uid) == 0);
    display = wl_display_connect(NULL);
    assert(display);
    struct wl_registry *registry = wl_display_get_registry(display);
    assert(wl_registry_add_listener(registry, &registry_listener, NULL) == 0);
    assert(wl_display_roundtrip(display) >= 0);
    assert(compositor && dmabuf && wm);
    struct buffer_info buffers_info[2] = { { .name = 'A' }, { .name = 'B' } };
    struct wl_buffer *buffers[2] = {
        import(bos[0], buffers_info), import(bos[1], buffers_info + 1),
    };
    assert(wl_display_roundtrip(display) >= 0);
    surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xdg = xdg_wm_base_get_xdg_surface(wm, surface);
    xdg_surface_add_listener(xdg, &xdg_listener, NULL);
    struct xdg_toplevel *toplevel = xdg_surface_get_toplevel(xdg);
    xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
    xdg_toplevel_set_title(toplevel, "asynchronous-buffer-release diagnostic");
    xdg_toplevel_set_app_id(toplevel, "niri-buffer-release-test");
    wl_surface_commit(surface);
    while (!configured) assert(wl_display_dispatch(display) >= 0);
    printf("%.6f READY buffers=A,B size=%dx%d modifier=LINEAR uid=%u\n",
           elapsed(), WIDTH, HEIGHT, (unsigned)getuid());

    for (;;) {
        assert(wl_display_dispatch_pending(display) >= 0);
        assert(wl_display_flush(display) >= 0 || errno == EAGAIN);
        struct pollfd fds[] = {
            { .fd = wl_display_get_fd(display), .events = POLLIN },
            { .fd = STDIN_FILENO, .events = POLLIN },
        };
        assert(poll(fds, 2, -1) > 0);
        if (fds[0].revents) assert(wl_display_dispatch(display) >= 0);
        if (fds[1].revents) {
            char command[32];
            assert(fgets(command, sizeof(command), stdin));
            if (!strcmp(command, "quit\n")) break;
            if (!strcmp(command, "repaint-A\n")) {
                struct buffer_info *info = buffers_info;
                if (info->released) {
                    paint(bos[0], egl, 0xff00ffff);
                    printf("%.6f REPAINT_A performed=1 color=00ffff latest_attach=%u\n",
                           elapsed(), info->latest_attach);
                } else {
                    printf("%.6f REPAINT_A performed=0 reason=unreleased latest_attach=%u\n",
                           elapsed(), info->latest_attach);
                }
                continue;
            }
            assert(!strcmp(command, "A\n") || !strcmp(command, "B\n"));
            unsigned index = command[0] - 'A';
            struct frame_info *info = malloc(sizeof(*info));
            assert(info);
            *info = (struct frame_info){ .sequence = ++sequence, .buffer = command[0] };
            buffers_info[index].released = false;
            buffers_info[index].latest_attach = sequence;
            struct wl_callback *callback = wl_surface_frame(surface);
            wl_callback_add_listener(callback, &frame_listener, info);
            wl_surface_attach(surface, buffers[index], 0, 0);
            wl_surface_damage_buffer(surface, 0, 0, WIDTH, HEIGHT);
            wl_surface_commit(surface);
            printf("%.6f COMMIT buffer=%c commit=%u\n", elapsed(), command[0], sequence);
            assert(wl_display_roundtrip(display) >= 0);
            printf("%.6f SYNC commit=%u\n", elapsed(), sequence);
        }
    }
    wl_display_disconnect(display);
    gbm_bo_destroy(bos[0]);
    gbm_bo_destroy(bos[1]);
    eglMakeCurrent(egl, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(egl, context);
    eglTerminate(egl);
    gbm_device_destroy(device);
    close(fd);
}
