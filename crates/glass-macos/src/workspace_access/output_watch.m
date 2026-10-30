#import <Foundation/Foundation.h>
#import <CoreServices/CoreServices.h>
#include <errno.h>

typedef void (*GlassOutputCallback)(void *, const char *const *, const uint32_t *, size_t);

@interface GlassOutputWatch : NSObject
@property(nonatomic) FSEventStreamRef stream;
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, strong) dispatch_queue_t control;
@property(nonatomic, strong) dispatch_semaphore_t pending;
@property(nonatomic) void *context;
@property(nonatomic) GlassOutputCallback callback;
@end
@implementation GlassOutputWatch
@end

static void changed(ConstFSEventStreamRef stream, void *info, size_t count,
                    void *paths, const FSEventStreamEventFlags flags[],
                    const FSEventStreamEventId ids[]) {
    (void)stream;
    (void)ids;
    GlassOutputWatch *watch = (__bridge GlassOutputWatch *)info;
    watch.callback(watch.context, paths, flags, count);
}

int glass_output_watch_start(const char *path, void *context,
                             GlassOutputCallback callback, void **out) {
    @autoreleasepool {
        CFStringRef text = CFStringCreateWithFileSystemRepresentation(NULL, path);
        if (!text) return EINVAL;
        CFArrayRef paths = CFArrayCreate(NULL, (const void **)&text, 1, &kCFTypeArrayCallBacks);
        CFRelease(text);
        if (!paths) return ENOMEM;
        GlassOutputWatch *watch = [GlassOutputWatch new];
        watch.context = context;
        watch.callback = callback;
        watch.queue = dispatch_queue_create("glass.output-access", DISPATCH_QUEUE_SERIAL);
        watch.control = dispatch_queue_create("glass.output-barrier", DISPATCH_QUEUE_SERIAL);
        FSEventStreamContext settings = {0, (__bridge void *)watch, NULL, NULL, NULL};
        watch.stream = FSEventStreamCreate(NULL, changed, &settings, paths,
            kFSEventStreamEventIdSinceNow, 0.05,
            kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagWatchRoot |
            kFSEventStreamCreateFlagIgnoreSelf | kFSEventStreamCreateFlagFileEvents);
        CFRelease(paths);
        if (!watch.stream) return EIO;
        FSEventStreamSetDispatchQueue(watch.stream, watch.queue);
        if (!FSEventStreamStart(watch.stream)) {
            FSEventStreamInvalidate(watch.stream);
            FSEventStreamRelease(watch.stream);
            return EIO;
        }
        *out = (__bridge_retained void *)watch;
        return 0;
    }
}

int glass_output_watch_flush(void *handle, uint64_t timeout_ns) {
    GlassOutputWatch *watch = (__bridge GlassOutputWatch *)handle;
    if (!watch.pending) {
        dispatch_semaphore_t completed = dispatch_semaphore_create(0);
        watch.pending = completed;
        dispatch_async(watch.control, ^{
            FSEventStreamFlushSync(watch.stream);
            dispatch_sync(watch.queue, ^{});
            dispatch_semaphore_signal(completed);
        });
    }
    if (dispatch_semaphore_wait(watch.pending,
        dispatch_time(DISPATCH_TIME_NOW, (int64_t)MIN(timeout_ns, INT64_MAX))) != 0) {
        return ETIMEDOUT;
    }
    watch.pending = nil;
    return 0;
}

void glass_output_watch_stop(void *handle) {
    GlassOutputWatch *watch = (__bridge_transfer GlassOutputWatch *)handle;
    dispatch_sync(watch.control, ^{});
    FSEventStreamStop(watch.stream);
    dispatch_sync(watch.queue, ^{});
    FSEventStreamInvalidate(watch.stream);
    FSEventStreamRelease(watch.stream);
}
