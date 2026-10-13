
            fun f() {
                try {
                    risky()
                } catch (e: IllegalStateException) {
                } catch (e: Exception) {
                } finally {
                    cleanup()
                }
            }
        