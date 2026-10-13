
            (defun f (n)
              (while (> n 0)
                (when (foo n) (bar n))
                (setq n (1- n))))
        