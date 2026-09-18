// Categories in a static archive member that defines no symbol anything
// references: only `-ObjC` loads it.
#import "objc_lib.h"

@implementation SuiteAnimal (Extra)

- (NSString *)extraInfo {
    return [NSString stringWithFormat:@"%@ has %ld legs", self.name, (long)self.legs];
}

@end

@implementation NSString (SuiteReverse)

- (NSString *)suite_reversed {
    NSMutableString *out = [NSMutableString stringWithCapacity:self.length];
    for (NSUInteger i = self.length; i > 0; i--)
        [out appendFormat:@"%C", [self characterAtIndex:i - 1]];
    return out;
}

@end
