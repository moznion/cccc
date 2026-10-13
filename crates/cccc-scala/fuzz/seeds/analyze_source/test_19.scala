
            import scala.util.control.Breaks._

            object S {
                def firstEven(xs: List[Int]): Int = {
                    var result = -1
                    breakable {
                        for (x <- xs) {
                            if (x % 2 == 0) {
                                result = x
                                break()
                            }
                        }
                    }
                    result
                }
            }
        