// Objective-C categories for category merging and relative method lists,
// without Foundation: a root class with categories (merged into it), two
// categories of NSObject from libobjc (merged into one), and a category
// with +load (left alone). Prints what the runtime sees.
int printf(const char *, ...);

// Base(Doubling) overrides -base on purpose.
#pragma clang diagnostic ignored "-Wobjc-protocol-method-implementation"

__attribute__((objc_root_class))
@interface NSObject
+ (id)new;
@end

@protocol Named
- (int)nameLength;
@end

@protocol Counted
- (int)count;
@end

__attribute__((objc_root_class))
@interface Base <Named> {
    int value;
}
@property int value;
+ (void)initialize;
+ (id)alloc;
- (int)base;
- (int)nameLength;
@end

@implementation Base
@synthesize value;
+ (void)initialize {
}
+ (id)alloc {
    return 0;
}
- (int)base {
    return 1;
}
- (int)nameLength {
    return 4;
}
@end

@interface Base (Doubling)
@property(readonly) int doubled;
+ (int)factor;
- (int)base;
@end

@implementation Base (Doubling)
+ (int)factor {
    return 2;
}
// Overrides the class's method: category methods come first.
- (int)base {
    return 10;
}
- (int)doubled {
    return 2;
}
@end

@interface Base (Tripling) <Counted>
+ (int)tripleFactor;
- (int)count;
@end

@implementation Base (Tripling)
+ (int)tripleFactor {
    return 3;
}
- (int)count {
    return 7;
}
@end

@interface Base (Loading)
@end

@implementation Base (Loading)
+ (void)load {
}
@end

@interface NSObject (First)
- (int)first;
@end

@implementation NSObject (First)
- (int)first {
    return 100;
}
@end

@interface NSObject (Second)
+ (int)second;
@end

@implementation NSObject (Second)
+ (int)second {
    return 200;
}
@end

int main(void) {
    int total = [Base factor] + [Base tripleFactor] + [NSObject second] + [[NSObject new] first];
    printf("categories %d\n", total);
    return total == 305 ? 0 : 1;
}
