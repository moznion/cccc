
object S:
  def f(): Int = try risky() catch case e: Exception => 1
