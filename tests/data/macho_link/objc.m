// Objective-C without Foundation: a root class with class methods, a
// category adding one, an instance variable and a selector reference.
int printf(const char *, ...);

__attribute__((objc_root_class))
@interface Greeter {
    int count;
}
+ (void)initialize;
+ (int)answer;
@end

@implementation Greeter
// A root class answers +initialize itself: there is no NSObject to.
+ (void)initialize {
}
+ (int)answer {
    return 40;
}
@end

@interface Greeter (Extra)
+ (int)extra;
@end

@implementation Greeter (Extra)
+ (int)extra {
    return 2;
}
@end

int main(void) {
    int value = [Greeter answer] + [Greeter extra];
    printf("objc %d\n", value);
    return value == 42 ? 0 : 1;
}
