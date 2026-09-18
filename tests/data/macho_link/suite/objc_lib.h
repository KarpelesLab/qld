// Interface of the Objective-C suite's dylib (objc_lib.m).
#import <Foundation/Foundation.h>

@protocol SuiteGreeting <NSObject>
- (NSString *)greet;
@optional
- (NSString *)farewell;
@end

@interface SuiteAnimal : NSObject
@property(nonatomic, copy) NSString *name;
@property(nonatomic) NSInteger legs;
- (instancetype)initWithName:(NSString *)name legs:(NSInteger)legs;
- (NSString *)describe;
+ (NSInteger)instances;
@end

// A category defined in the dylib, on the dylib's own class.
@interface SuiteAnimal (Sound)
- (NSString *)sound;
@end

// Categories from the static archive (objc_category.m), which only
// `-ObjC` loads: nothing references a symbol of that member.
@interface SuiteAnimal (Extra)
- (NSString *)extraInfo;
@end

@interface NSString (SuiteReverse)
- (NSString *)suite_reversed;
@end

FOUNDATION_EXPORT NSString *const SuiteLibraryName;
FOUNDATION_EXPORT NSInteger SuiteLibraryLoads;
FOUNDATION_EXPORT void SuiteThrow(NSString *reason);
