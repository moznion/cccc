
            object S {
                def f(o: Any, p: Boolean): String = o match {
                    case i: Int if i > 0 && p => "positive int"
                    case s: String => "string"
                    case _ => "other"
                }
            }
        