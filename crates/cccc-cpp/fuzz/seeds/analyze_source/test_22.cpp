
            void log_uptime(uint32_t up_time) {
                if (up_time) {
                    log_info("Uptime: %" PRIu32 "\r\n", up_time);
                }
            }
            