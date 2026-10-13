
object S:
  extension (x: Int) def isPos: Boolean = if x > 0 then true else false

  given Ordering[Int] with
    def compare(a: Int, b: Int): Int = if a < b then -1 else 1
