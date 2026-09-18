// The Objective-C suite's dylib (libsuite_objc.dylib): a class the
// executable subclasses, a category on it, a +load method, and an
// exception raised to the executable.
#import "objc_lib.h"

NSString *const SuiteLibraryName = @"libsuite_objc";
NSInteger SuiteLibraryLoads;

static NSInteger instances;

@implementation SuiteAnimal

+ (void)load {
    SuiteLibraryLoads++;
}

- (instancetype)initWithName:(NSString *)name legs:(NSInteger)legs {
    if ((self = [super init])) {
        _name = [name copy];
        _legs = legs;
        instances++;
    }
    return self;
}

- (NSString *)describe {
    return [NSString stringWithFormat:@"%@/%ld", self.name, (long)self.legs];
}

+ (NSInteger)instances {
    return instances;
}

@end

@implementation SuiteAnimal (Sound)

- (NSString *)sound {
    return @"...";
}

@end

void SuiteThrow(NSString *reason) {
    @throw [NSException exceptionWithName:@"SuiteException" reason:reason userInfo:@{@"code" : @7}];
}
