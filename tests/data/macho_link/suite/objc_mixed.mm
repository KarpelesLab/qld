// The Objective-C++ part of the `-fuse-ld` suite: C++ objects as instance
// variables (`.cxx_construct` and `.cxx_destruct`), C++ exceptions through
// Objective-C methods, and Objective-C exceptions caught by C++ handlers.
#import "objc_lib.h"

#include <cstdio>
#include <stdexcept>
#include <string>
#include <vector>

static int destroyed;

struct Member {
    std::vector<int> values{1, 2, 3};
    ~Member() { destroyed++; }
};

@interface SuiteHolder : NSObject {
    Member member;
}
- (int)sum;
- (void)throwCxx;
@end

@implementation SuiteHolder

- (int)sum {
    int total = 0;
    for (int value : member.values)
        total += value;
    return total;
}

- (void)throwCxx {
    throw std::runtime_error("from a method");
}

@end

#define CHECK(cond)                                                                \
    do {                                                                           \
        (*checks)++;                                                               \
        if (!(cond)) {                                                             \
            failures++;                                                            \
            std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);            \
        }                                                                          \
    } while (0)

extern "C" int SuiteMixedChecks(int *checks) {
    int failures = 0;
    @autoreleasepool {
        SuiteHolder *holder = [[SuiteHolder alloc] init];
        CHECK([holder sum] == 6);
        std::string message;
        try {
            [holder throwCxx];
        } catch (const std::exception &error) {
            message = error.what();
        }
        CHECK(message == "from a method");
        holder = nil;
    }
    CHECK(destroyed == 1);

    bool caught = false;
    try {
        SuiteThrow(@"to C++");
    } catch (NSException *exception) {
        caught = [exception.reason isEqualToString:@"to C++"];
    }
    CHECK(caught);

    caught = false;
    @try {
        throw std::logic_error("c++ in @try");
    } @catch (NSException *exception) {
        caught = false;
    } @catch (...) {
        caught = true;
    }
    CHECK(caught);
    return failures;
}
