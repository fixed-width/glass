// virtual_display.m — glass macOS validation, step 1.
//
// Creates an off-screen virtual display via the PRIVATE CoreGraphics CGVirtualDisplay
// API — the v1 display-provisioning path in the macOS backend design spec. Proves a
// headless (monitorless) Mac can host a display that ScreenCaptureKit can later capture.
//
//   Build:  clang -fobjc-arc -framework Foundation -framework CoreGraphics \
//                 -o virtualdisplay virtual_display.m
//   Run:    ./virtualdisplay [width] [height]      (default 1920x1080)
//           -> creates the display, prints its id, and HOLDS IT OPEN until Ctrl-C
//              (the display exists only while this process lives). Run capture_window
//              in another shell while this is up.
//
// ⚠️  CGVirtualDisplay is PRIVATE and UNDOCUMENTED. The @interface declarations below
// mirror the public reverse-engineered headers, but the exact property/method set
// DRIFTS across macOS releases. If this fails to compile, or the display does not
// appear in the "after" list, cross-check the current interface against and copy the
// field set from:
//   - https://github.com/enfp-dev-studio/node-mac-virtual-display  (src/virtual_display.mm, MIT)
//   - https://github.com/w0lfschild/macOS_headers                  (.../CoreGraphics/.../CGVirtualDisplay.h)
//   - Chromium ui/display/mac/test/virtual_display_mac_util.mm     (BSD)
// Adjusting these declarations per macOS version is expected — it is exactly the
// "per-release maintenance tax" the design spec calls out.

#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>

@interface CGVirtualDisplayDescriptor : NSObject
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, copy)   NSString *name;
@property(nonatomic, assign) uint32_t maxPixelsWide;
@property(nonatomic, assign) uint32_t maxPixelsHigh;
@property(nonatomic, assign) CGSize   sizeInMillimeters;
@property(nonatomic, assign) uint32_t productID;
@property(nonatomic, assign) uint32_t vendorID;
@property(nonatomic, assign) uint32_t serialNum;
@property(nonatomic, copy)   void (^terminationHandler)(void);
@end

@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(uint32_t)width height:(uint32_t)height refreshRate:(double)refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(nonatomic, strong) NSArray<CGVirtualDisplayMode *> *modes;
@property(nonatomic, assign) uint32_t hiDPI;
@end

@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@property(nonatomic, readonly) CGDirectDisplayID displayID;
@end

static void printActiveDisplays(const char *when) {
    uint32_t count = 0;
    CGGetActiveDisplayList(0, NULL, &count);
    CGDirectDisplayID ids[32];
    if (count > 32) count = 32;
    CGGetActiveDisplayList(count, ids, &count);
    NSLog(@"%s: %u active display(s)", when, count);
    for (uint32_t i = 0; i < count; i++) {
        NSLog(@"    id=%u  %zux%zu", ids[i],
              CGDisplayPixelsWide(ids[i]), CGDisplayPixelsHigh(ids[i]));
    }
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        uint32_t width  = (argc > 1) ? (uint32_t)atoi(argv[1]) : 1920;
        uint32_t height = (argc > 2) ? (uint32_t)atoi(argv[2]) : 1080;
        uint32_t hidpi  = (argc > 3) ? (uint32_t)atoi(argv[3]) : 0;  // 0 = 1x, 1 = Retina 2x

        printActiveDisplays("before");

        CGVirtualDisplayDescriptor *desc = [CGVirtualDisplayDescriptor new];
        desc.queue = dispatch_get_main_queue();
        desc.name = @"glass-validation";
        desc.maxPixelsWide = width;
        desc.maxPixelsHigh = height;
        desc.sizeInMillimeters = CGSizeMake(600, 340); // arbitrary ~27" 16:9
        desc.vendorID  = 0xEEEE;
        desc.productID = 0x0001;
        desc.serialNum = 0x0001;
        desc.terminationHandler = ^{ NSLog(@"virtual display terminated"); };

        CGVirtualDisplay *display = [[CGVirtualDisplay alloc] initWithDescriptor:desc];
        if (!display) { NSLog(@"FAIL: CGVirtualDisplay init returned nil"); return 1; }

        CGVirtualDisplayMode *mode =
            [[CGVirtualDisplayMode alloc] initWithWidth:width height:height refreshRate:60.0];
        CGVirtualDisplaySettings *settings = [CGVirtualDisplaySettings new];
        settings.modes = @[mode];
        settings.hiDPI = hidpi;

        if (![display applySettings:settings]) { NSLog(@"FAIL: applySettings returned NO"); return 1; }

        CGDirectDisplayID did = display.displayID;
        CGDisplayModeRef m = CGDisplayCopyDisplayMode(did);
        size_t pxW = m ? CGDisplayModeGetPixelWidth(m) : 0, ptW = m ? CGDisplayModeGetWidth(m) : 0;
        if (m) CGDisplayModeRelease(m);
        NSLog(@"OK: created virtual display id=%u (%ux%u, hiDPI=%u) — mode points=%zu pixels=%zu (scale ~%.1f)",
              did, width, height, hidpi, ptW, pxW, ptW ? (double)pxW / (double)ptW : 0.0);
        printActiveDisplays("after");
        NSLog(@"holding display open — Ctrl-C to release; run capture_window now.");
        dispatch_main(); // keep the process (and thus the display) alive
    }
    return 0;
}
