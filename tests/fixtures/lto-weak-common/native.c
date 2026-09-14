const char *overridden_lto_qld(void) { return "strong native"; }

__attribute__((weak)) const char *ir_strong_lto_qld(void) { return "weak native"; }

const char *native_weak_caller_lto_qld(void) { return ir_strong_lto_qld(); }

int common_counter_lto_qld;
char common_buffer_lto_qld[64];

void native_touch_commons_lto_qld(void)
{
    common_counter_lto_qld += 4;
    common_buffer_lto_qld[63] = 'x';
}

const char *common_same_lto_qld(char *buffer)
{
    return buffer == common_buffer_lto_qld && buffer[63] == 'x' ? "yes" : "no";
}
