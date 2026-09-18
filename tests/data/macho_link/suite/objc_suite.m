// The Objective-C part of the `-fuse-ld` suite (tests/macho_link/suite.rs):
// a Foundation program built with Apple clang, ARC and qld against the
// real SDK. A subclass of a dylib's class, categories (from this image,
// the dylib, and a static archive loaded by `-ObjC`), protocols, +load,
// blocks, Grand Central Dispatch, exceptions across images, weak
// references, key-value coding, notifications, and Objective-C++
// (objc_mixed.mm).
#import "objc_lib.h"

#include <stdio.h>

static int checks;
static int failures;

#define CHECK(cond)                                                                \
    do {                                                                           \
        checks++;                                                                  \
        if (!(cond)) {                                                             \
            failures++;                                                            \
            printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);                 \
        }                                                                          \
    } while (0)

// From objc_mixed.mm.
int SuiteMixedChecks(int *checks);

static int load_calls;

@interface SuiteDog : SuiteAnimal <SuiteGreeting>
@property(nonatomic) NSInteger tricks;
@end

@implementation SuiteDog

+ (void)load {
    load_calls++;
}

- (NSString *)describe {
    return [NSString stringWithFormat:@"dog %@", [super describe]];
}

- (NSString *)greet {
    return [NSString stringWithFormat:@"woof from %@", self.name];
}

@end

// Categories on this image's class: the linker may merge them into it.
@interface SuiteDog (Tricks)
- (NSInteger)learn;
@end

@implementation SuiteDog (Tricks)

+ (void)load {
    load_calls += 10;
}

- (NSInteger)learn {
    return ++self.tricks;
}

@end

@interface SuiteDog (Sound)
@end

@implementation SuiteDog (Sound)

- (NSString *)sound {
    return @"woof";
}

@end

@interface SuiteDog (Class)
+ (NSString *)species;
@property(nonatomic, readonly) NSString *shout;
@end

@implementation SuiteDog (Class)

+ (NSString *)species {
    return @"canis";
}

- (NSString *)shout {
    return [self.sound uppercaseString];
}

@end

@interface SuiteObserver : NSObject
@property(nonatomic) NSInteger notifications;
@end

@implementation SuiteObserver

- (void)notified:(NSNotification *)notification {
    self.notifications += [notification.userInfo[@"count"] integerValue];
}

@end

static void check_classes(void) {
    CHECK(load_calls == 11);
    CHECK(SuiteLibraryLoads == 1);
    CHECK([SuiteLibraryName isEqualToString:@"libsuite_objc"]);

    SuiteDog *dog = [[SuiteDog alloc] initWithName:@"rex" legs:4];
    CHECK([[dog describe] isEqualToString:@"dog rex/4"]);
    CHECK([[dog greet] isEqualToString:@"woof from rex"]);
    CHECK([dog conformsToProtocol:@protocol(SuiteGreeting)]);
    CHECK(![dog respondsToSelector:@selector(farewell)]);
    CHECK([dog isKindOfClass:[SuiteAnimal class]]);
    CHECK([SuiteDog superclass] == [SuiteAnimal class]);
    CHECK([SuiteAnimal instances] >= 1);

    // Categories.
    CHECK([dog learn] == 1 && [dog learn] == 2);
    CHECK([[dog sound] isEqualToString:@"woof"]);
    CHECK([[dog shout] isEqualToString:@"WOOF"]);
    CHECK([[SuiteDog species] isEqualToString:@"canis"]);
    SuiteAnimal *cat = [[SuiteAnimal alloc] initWithName:@"tom" legs:4];
    CHECK([[cat sound] isEqualToString:@"..."]);
    CHECK([[cat extraInfo] isEqualToString:@"tom has 4 legs"]);
    CHECK([[@"qld" suite_reversed] isEqualToString:@"dlq"]);

    // The runtime knows the classes by name.
    CHECK(NSClassFromString(@"SuiteDog") == [SuiteDog class]);
    CHECK([NSStringFromClass([dog class]) isEqualToString:@"SuiteDog"]);
    CHECK([dog respondsToSelector:NSSelectorFromString(@"learn")]);

    // Key-value coding.
    CHECK([[dog valueForKey:@"name"] isEqualToString:@"rex"]);
    [dog setValue:@3 forKey:@"legs"];
    CHECK(dog.legs == 3);
}

static void check_foundation(void) {
    NSArray *numbers = @[ @5, @3, @9, @1 ];
    NSArray *sorted = [numbers sortedArrayUsingComparator:^NSComparisonResult(NSNumber *a, NSNumber *b) {
      return [a compare:b];
    }];
    CHECK([sorted isEqualToArray:(@[ @1, @3, @5, @9 ])]);
    __block NSInteger total = 0;
    [numbers enumerateObjectsUsingBlock:^(NSNumber *number, NSUInteger index, BOOL *stop) {
      total += number.integerValue;
    }];
    CHECK(total == 18);
    NSDictionary *map = @{@"a" : @1, @"b" : @2};
    CHECK([map[@"b"] isEqual:@2] && map.count == 2);
    NSString *joined = [[@"x,y,z" componentsSeparatedByString:@","] componentsJoinedByString:@"-"];
    CHECK([joined isEqualToString:@"x-y-z"]);
    NSData *data = [@"hello" dataUsingEncoding:NSUTF8StringEncoding];
    CHECK(data.length == 5);
    NSString *formatted = [NSString stringWithFormat:@"%@ %d %.1f", @"v", 42, 2.5];
    CHECK([formatted isEqualToString:@"v 42 2.5"]);

    // Heap blocks capturing objects, kept in a collection.
    NSMutableArray *blocks = [NSMutableArray array];
    for (NSInteger i = 0; i < 3; i++) {
        NSString *label = [NSString stringWithFormat:@"b%ld", (long)i];
        [blocks addObject:[^NSString * { return label; } copy]];
    }
    NSString *(^second)(void) = blocks[1];
    CHECK([second() isEqualToString:@"b1"]);

    // Weak references.
    __weak id weak;
    @autoreleasepool {
        NSObject *object = [[NSObject alloc] init];
        weak = object;
        CHECK(weak != nil);
        object = nil;
    }
    CHECK(weak == nil);

    // Notifications.
    SuiteObserver *observer = [[SuiteObserver alloc] init];
    NSNotificationCenter *center = [NSNotificationCenter defaultCenter];
    [center addObserver:observer selector:@selector(notified:) name:@"SuiteNote" object:nil];
    [center postNotificationName:@"SuiteNote" object:nil userInfo:@{@"count" : @4}];
    [center removeObserver:observer];
    CHECK(observer.notifications == 4);
}

static void check_exceptions(void) {
    BOOL caught = NO;
    BOOL finally = NO;
    @try {
        SuiteThrow(@"from the dylib");
    } @catch (NSException *exception) {
        caught = [exception.name isEqualToString:@"SuiteException"] &&
                 [exception.reason isEqualToString:@"from the dylib"] &&
                 [exception.userInfo[@"code"] isEqual:@7];
    } @finally {
        finally = YES;
    }
    CHECK(caught && finally);

    caught = NO;
    @try {
        (void)[@[] objectAtIndex:3];
    } @catch (NSException *exception) {
        caught = [exception.name isEqualToString:NSRangeException];
    }
    CHECK(caught);

    NSObject *lock = [[NSObject alloc] init];
    NSInteger inside = 0;
    @synchronized(lock) {
        inside = 1;
    }
    CHECK(inside == 1);
}

static void check_dispatch(void) {
    dispatch_queue_t queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0);
    dispatch_group_t group = dispatch_group_create();
    __block int64_t sum = 0;
    dispatch_queue_t serial = dispatch_queue_create("qld.suite", DISPATCH_QUEUE_SERIAL);
    for (int i = 1; i <= 100; i++) {
        dispatch_group_async(group, queue, ^{
          dispatch_sync(serial, ^{
            sum += i;
          });
        });
    }
    dispatch_group_wait(group, DISPATCH_TIME_FOREVER);
    CHECK(sum == 5050);

    static dispatch_once_t once;
    __block int once_calls = 0;
    for (int i = 0; i < 3; i++)
        dispatch_once(&once, ^{
          once_calls++;
        });
    CHECK(once_calls == 1);
}

int main(void) {
    @autoreleasepool {
        check_classes();
        check_foundation();
        check_exceptions();
        check_dispatch();
        int mixed_checks = 0;
        failures += SuiteMixedChecks(&mixed_checks);
        checks += mixed_checks;
        printf("objc suite: %d checks, %d failures\n", checks, failures);
    }
    return failures == 0 ? 0 : 1;
}
