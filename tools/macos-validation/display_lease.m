// Bounded display-lifecycle probe. Creates only owned displays and posts no input.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <unistd.h>
#include <string.h>

@interface CGVirtualDisplayDescriptor : NSObject
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, copy) NSString *name;
@property(nonatomic) uint32_t maxPixelsWide;
@property(nonatomic) uint32_t maxPixelsHigh;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) uint32_t productID;
@property(nonatomic) uint32_t vendorID;
@property(nonatomic) uint32_t serialNum;
@end
@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(uint32_t)width height:(uint32_t)height refreshRate:(double)refreshRate;
@end
@interface CGVirtualDisplaySettings : NSObject
@property(nonatomic, strong) NSArray *modes;
@property(nonatomic) uint32_t hiDPI;
@end
@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@property(nonatomic, readonly) CGDirectDisplayID displayID;
@end

static NSArray *rectValues(CGRect rect) {
    return @[@(rect.origin.x), @(rect.origin.y), @(rect.size.width), @(rect.size.height)];
}

static NSArray *displayList(BOOL online) {
    CGDirectDisplayID ids[64];
    uint32_t count = 0;
    CGError result = online ? CGGetOnlineDisplayList(64, ids, &count)
                            : CGGetActiveDisplayList(64, ids, &count);
    if (result != kCGErrorSuccess || count >= 64) return nil;
    NSMutableArray *values = [NSMutableArray array];
    for (uint32_t i = 0; i < count; ++i) {
        CGDirectDisplayID id = ids[i];
        CGDisplayModeRef mode = CGDisplayCopyDisplayMode(id);
        if (!mode) return nil;
        [values addObject:@{
            @"id": @(id), @"bounds": rectValues(CGDisplayBounds(id)),
            @"mode": @[@(CGDisplayModeGetWidth(mode)), @(CGDisplayModeGetHeight(mode)),
                       @(CGDisplayModeGetPixelWidth(mode)), @(CGDisplayModeGetPixelHeight(mode)),
                       @(CGDisplayModeGetRefreshRate(mode))],
            @"mirrors": @(CGDisplayMirrorsDisplay(id)),
            @"model": @(CGDisplayModelNumber(id)), @"vendor": @(CGDisplayVendorNumber(id))
        }];
        CGDisplayModeRelease(mode);
    }
    return values;
}

static NSDictionary *snapshot(void) {
    NSArray *active = displayList(NO), *online = displayList(YES);
    if (!active || !online) return nil;
    NSMutableArray *screens = [NSMutableArray array];
    for (NSScreen *screen in NSScreen.screens) {
        NSNumber *id = screen.deviceDescription[@"NSScreenNumber"];
        if (id) [screens addObject:@{@"id": id, @"frame": rectValues(screen.frame)}];
    }
    NSMutableArray *windows = [NSMutableArray array];
    CFArrayRef copied = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
    if (!copied) return nil;
    for (NSDictionary *window in (__bridge NSArray *)copied) {
        [windows addObject:@{
            @"id": window[(__bridge NSString *)kCGWindowNumber],
            @"pid": window[(__bridge NSString *)kCGWindowOwnerPID],
            @"layer": window[(__bridge NSString *)kCGWindowLayer],
            @"bounds": window[(__bridge NSString *)kCGWindowBounds]
        }];
    }
    CFRelease(copied);
    CGEventRef event = CGEventCreate(NULL);
    if (!event) return nil;
    CGPoint cursor = CGEventGetLocation(event);
    CFRelease(event);
    return @{@"main": @(CGMainDisplayID()), @"active": active, @"online": online,
             @"screens": screens, @"windows": windows,
             @"foreground_pid": @(NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier),
             @"cursor": @[@(cursor.x), @(cursor.y)]};
}

static void emit(NSString *phase, NSDictionary *extra) {
    NSMutableDictionary *record = [@{@"phase": phase, @"at": @(NSProcessInfo.processInfo.systemUptime)} mutableCopy];
    NSDictionary *facts = snapshot();
    record[@"facts"] = facts ?: @{@"inventory_failed": @YES};
    [record addEntriesFromDictionary:extra];
    NSData *data = [NSJSONSerialization dataWithJSONObject:record options:NSJSONWritingSortedKeys error:NULL];
    puts([[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding].UTF8String);
    fflush(stdout);
}

static void pump(NSTimeInterval duration) {
    NSDate *end = [NSDate dateWithTimeIntervalSinceNow:duration];
    do {
        @autoreleasepool {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                untilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]
                inMode:NSDefaultRunLoopMode dequeue:YES];
            if (event) [NSApp sendEvent:event];
            [NSApp updateWindows];
        }
    } while (end.timeIntervalSinceNow > 0);
}

static BOOL registered(CGDirectDisplayID wanted) {
    NSArray *online = displayList(YES);
    BOOL found = NO;
    for (NSDictionary *row in online) if ([row[@"id"] unsignedIntValue] == wanted) found = YES;
    if (!online || !found) return NO;
    for (NSScreen *screen in NSScreen.screens)
        if ([screen.deviceDescription[@"NSScreenNumber"] unsignedIntValue] == wanted) return YES;
    return NO;
}

static CGVirtualDisplay *create(uint32_t width, uint32_t height, uint32_t serial) {
    @autoreleasepool {
        CGVirtualDisplayDescriptor *descriptor = [CGVirtualDisplayDescriptor new];
        descriptor.queue = dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0);
        descriptor.name = @"glass-lifecycle-probe";
        descriptor.maxPixelsWide = width;
        descriptor.maxPixelsHigh = height;
        descriptor.sizeInMillimeters = CGSizeMake(500, 300);
        descriptor.vendorID = 0xEEEE;
        descriptor.productID = 2;
        descriptor.serialNum = serial;
        CGVirtualDisplay *display = [[CGVirtualDisplay alloc] initWithDescriptor:descriptor];
        CGVirtualDisplaySettings *settings = [CGVirtualDisplaySettings new];
        settings.hiDPI = 0;
        settings.modes = @[[[CGVirtualDisplayMode alloc] initWithWidth:width height:height refreshRate:60]];
        if (!display || ![display applySettings:settings]) return nil;
        return display;
    }
}

static BOOL awaitRegistration(CGDirectDisplayID id) {
    NSTimeInterval end = NSProcessInfo.processInfo.systemUptime + 5;
    while (!registered(id) && NSProcessInfo.processInfo.systemUptime < end) pump(0.02);
    return registered(id);
}

static BOOL preserved(NSDictionary *baseline) {
    NSDictionary *current = snapshot();
    return current && [current[@"main"] isEqual:baseline[@"main"]]
        && [current[@"active"] containsObject:[baseline[@"active"] firstObject]];
}

static BOOL windowsPreserved(NSDictionary *baseline) {
    NSDictionary *current = snapshot();
    if (!current) return NO;
    for (NSDictionary *window in baseline[@"windows"]) {
        if ([window[@"layer"] intValue] != 0) continue;
        if (![current[@"windows"] containsObject:window]) return NO;
    }
    return YES;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 2) { fprintf(stderr, "usage: display_lease /absolute/path/to/release-file\n"); return 2; }
        NSApplication *app = NSApplication.sharedApplication;
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        if (strcmp(argv[1], "--inspect") == 0) {
            emit(@"inventory", @{});
            return 0;
        }
        NSString *release = [NSString stringWithUTF8String:argv[1]];
        if (!release.isAbsolutePath || [NSFileManager.defaultManager fileExistsAtPath:release]) return 2;
        for (NSString *name in @[@"CGVirtualDisplayDescriptor", @"CGVirtualDisplayMode",
                                @"CGVirtualDisplaySettings", @"CGVirtualDisplay"])
            if (!NSClassFromString(name)) { emit(@"missing-class", @{@"name": name}); return 1; }
        NSDictionary *baseline = snapshot();
        emit(@"baseline", @{});
        if (!baseline || [baseline[@"online"] count] == 0) return 1;
        BOOL fallbackOnly = YES;
        for (NSDictionary *display in baseline[@"online"])
            if ([display[@"model"] unsignedIntValue] != 0x76697274) fallbackOnly = NO;
        if ([baseline[@"active"] count] != 1) { emit(@"unsupported-baseline", @{}); return 1; }
        __attribute__((objc_precise_lifetime)) CGVirtualDisplay *anchor = nil, *secondary = nil;
        CGDirectDisplayID anchorID = 0, secondaryID = 0;
        int result = 1;
        do {
        if (fallbackOnly) {
            NSArray *mode = baseline[@"active"][0][@"mode"];
            anchor = create([mode[0] unsignedIntValue], [mode[1] unsignedIntValue], (uint32_t)getpid());
            anchorID = anchor.displayID;
            if (!anchor || !awaitRegistration(anchorID)) break;
            emit(@"anchor-ready", @{@"owned_id": @(anchorID)});
        }
        NSDictionary *anchorBaseline = snapshot();
        secondary = create(1280, 900, (uint32_t)getpid() + 1);
        secondaryID = secondary.displayID;
        if (!secondary || !awaitRegistration(secondaryID)) break;
        emit(@"secondary-registered", @{@"owned_id": @(secondaryID)});
        if (!preserved(anchorBaseline)) { emit(@"primary-changed", @{}); break; }
        CGRect main = CGDisplayBounds(CGMainDisplayID());
        CGDisplayConfigRef config = NULL;
        CGError begin = CGBeginDisplayConfiguration(&config);
        if (begin != kCGErrorSuccess || !config) break;
        CGError place = CGConfigureDisplayOrigin(config, secondaryID,
            (int32_t)CGRectGetMaxX(main), (int32_t)CGRectGetMaxY(main));
        if (place != kCGErrorSuccess) { CGCancelDisplayConfiguration(config); break; }
        CGError commit = CGCompleteDisplayConfiguration(config, kCGConfigureForSession);
        if (commit != kCGErrorSuccess) { CGCancelDisplayConfiguration(config); break; }
        pump(0.2);
        if (!preserved(anchorBaseline)) { emit(@"primary-changed", @{}); break; }
        if (!windowsPreserved(baseline)) { emit(@"foreign-windows-changed", @{}); break; }
        emit(@"ready", @{@"owned_anchor": @(anchorID), @"owned_secondary": @(secondaryID),
                       @"fallback_only": @(fallbackOnly), @"foreign_windows_preserved": @YES,
                       @"input_posted": @NO});
        NSTimeInterval end = NSProcessInfo.processInfo.systemUptime + 45;
        while (![NSFileManager.defaultManager fileExistsAtPath:release]
               && NSProcessInfo.processInfo.systemUptime < end) pump(0.05);
        emit(@"before-release", @{});
        result = 0;
        } while (NO);
        secondary = nil;
        anchor = nil;
        NSTimeInterval endRemoval = NSProcessInfo.processInfo.systemUptime + 5;
        while (NSProcessInfo.processInfo.systemUptime < endRemoval) {
            NSArray *online = displayList(YES);
            BOOL remains = !online;
            for (NSDictionary *row in online) {
                uint32_t id = [row[@"id"] unsignedIntValue];
                if (id == secondaryID || id == anchorID) remains = YES;
            }
            if (!remains) break;
            pump(0.05);
        }
        emit(@"released", @{@"owned_anchor": @(anchorID), @"owned_secondary": @(secondaryID),
                          @"exit_code": @(result)});
        return result;
    }
}
