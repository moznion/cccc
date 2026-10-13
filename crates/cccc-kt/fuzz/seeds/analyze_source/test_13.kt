
            @Composable
            fun Loader(
                loading: @Composable (Scope.(State) -> Unit)? = null,
                content: @Composable (String) -> Unit,
            ) {
                if (loading != null) {
                    content("x")
                }
            }
        