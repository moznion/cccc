
            object S {
                def f(): Unit = {
                    try {
                        risky()
                    } catch {
                        case e: IllegalStateException =>
                        case e: RuntimeException =>
                    } finally {
                        cleanup()
                    }
                }
            }
        