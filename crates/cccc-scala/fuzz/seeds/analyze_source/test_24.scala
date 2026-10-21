
            object S {
                def f(n: Int, p: Boolean): String = n match {
                    case 1 => "one"
                    case _ if p => "guarded"
                    case _ => "other"
                }
            }
        