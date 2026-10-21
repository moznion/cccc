
            (defmethod handle :before ((obj account) amount)
              (when (minusp amount) (error "no")))
        